//! Rust class boxes (#452 slice 2), from the file's own syntax only.
//!
//! `struct`, `enum`, `union` and `trait` items are boxes. An `impl` block is a
//! contribution to the class whose qualified name its self type resolves to through
//! this file's `use`/`mod` scopes; composition attaches it (Decision 0004's global
//! step). No macro expansion, cfg evaluation, re-export following, type inference or
//! bare-name guessing across files. Every relation stays a terminal `unmatched` hint.
use super::*;

/// Crate key and module path of a workspace-relative `.rs` path under the
/// conventional Cargo layout: `<prefix>/src/lib.rs` and `main.rs` are the crate
/// root of crate `<prefix>`, `src/a/b.rs` and `src/a/b/mod.rs` are module `a::b`,
/// `src/bin/x.rs` and any file outside `src/` (tests, examples, `build.rs`) are
/// their own crate roots, keyed by path. `#[path]` and `include!` are not followed.
pub(super) fn crate_module(path: &str) -> (String, Vec<String>) {
    let stem = path.strip_suffix(".rs").unwrap_or(path);
    let parts: Vec<&str> = stem.split('/').collect();
    let module = |rest: &[&str]| -> Vec<String> {
        let mut rest: Vec<String> = rest.iter().map(|s| (*s).to_owned()).collect();
        if rest.last().is_some_and(|last| last == "mod") {
            rest.pop();
        }
        rest
    };
    let Some(src) = parts[..parts.len().saturating_sub(1)]
        .iter()
        .rposition(|part| *part == "src")
    else {
        return (path.to_owned(), Vec::new());
    };
    let prefix = parts[..src].join("/");
    let rest = &parts[src + 1..];
    if rest.first() == Some(&"bin") && rest.len() >= 2 {
        let root = parts[..src + 3].join("/");
        if rest.len() == 2 {
            return (format!("{root}.rs"), Vec::new());
        }
        let inner = module(&rest[2..]);
        let inner = if inner == ["main"] { Vec::new() } else { inner };
        return (format!("{root}/main.rs"), inner);
    }
    if rest == ["lib"] || rest == ["main"] {
        return (prefix, Vec::new());
    }
    (prefix, module(rest))
}

/// `crate-key::module::Name`; the root crate (`src/` at the workspace root) has
/// no prefix. Keys never collide: module names cannot contain `/` or `.`.
pub(super) fn qualified(krate: &str, module: &[String], name: &str) -> String {
    let mut parts: Vec<&str> = module.iter().map(String::as_str).collect();
    parts.push(name);
    let inner = parts.join("::");
    if krate.is_empty() {
        inner
    } else {
        format!("{krate}::{inner}")
    }
}

fn ident(segment: &str) -> bool {
    let segment = segment.strip_prefix("r#").unwrap_or(segment);
    !segment.is_empty()
        && segment
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c.is_alphabetic() || (i > 0 && c.is_numeric()))
}

/// Path segments of path text; `None` for anything but plain `a::b::C` spellings.
/// A leading `::` (an extern path) yields an empty first segment.
fn segments(text: &str) -> Option<Vec<String>> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let (global, body) = match compact.strip_prefix("::") {
        Some(body) => (true, body),
        None => (false, compact.as_str()),
    };
    let mut out: Vec<String> = if global { vec![String::new()] } else { vec![] };
    for part in body.split("::") {
        if !ident(part) {
            return None;
        }
        out.push(part.strip_prefix("r#").unwrap_or(part).to_owned());
    }
    Some(out)
}

#[derive(Default)]
struct ModScope {
    path: Vec<String>,
    /// Declared struct/enum/union/trait names (cfg duplicates counted).
    types: BTreeMap<String, usize>,
    /// Type aliases are not classes and never resolve to one.
    aliases: BTreeSet<String>,
    mods: BTreeSet<String>,
    /// `use` bindings: name → full path, `None` for an unresolvable spelling.
    uses: BTreeMap<String, Vec<Option<Vec<String>>>>,
    globs: Vec<Vec<String>>,
    /// Inline child modules by their `mod_item` start byte.
    children: BTreeMap<usize, usize>,
}

/// Per-file resolution work bound. Globs can point at each other, so every
/// resolution step is counted; past the bound nothing more resolves.
const RESOLVE_STEPS: usize = 10_000;
struct Scopes(Vec<ModScope>, std::cell::Cell<usize>);
impl Scopes {
    /// Absolute module path plus final name, or `None` when the spelling is
    /// external, ambiguous, an alias or otherwise not provable from this file.
    fn resolve(
        &self,
        scope: usize,
        segs: &[String],
        module: bool,
        depth: usize,
    ) -> Option<Vec<String>> {
        self.1.set(self.1.get() + 1);
        if depth > 8 || segs.is_empty() || self.1.get() > RESOLVE_STEPS {
            return None;
        }
        let s = &self.0[scope];
        let walk = |mut base: Vec<String>, rest: &[String]| -> Option<Vec<String>> {
            for seg in rest {
                match seg.as_str() {
                    "super" => {
                        base.pop()?;
                    }
                    "self" | "crate" | "Self" | "" => return None,
                    _ => base.push(seg.clone()),
                }
            }
            Some(base)
        };
        match segs[0].as_str() {
            "crate" => walk(Vec::new(), &segs[1..]),
            "self" => walk(s.path.clone(), &segs[1..]),
            "super" => {
                let mut base = s.path.clone();
                base.pop()?;
                walk(base, &segs[1..])
            }
            "Self" | "" => None,
            first => {
                let last = segs.len() == 1;
                if s.aliases.contains(first) {
                    return None;
                }
                if last && !module && s.types.contains_key(first) {
                    let mut path = s.path.clone();
                    path.push(first.to_owned());
                    return Some(path);
                }
                if (!last || module) && s.mods.contains(first) {
                    let mut path = s.path.clone();
                    path.push(first.to_owned());
                    return walk(path, &segs[1..]);
                }
                if s.types.contains_key(first) || s.mods.contains(first) {
                    // `Type::Assoc`, or a module where a type is required.
                    return None;
                }
                if let Some(bindings) = s.uses.get(first) {
                    let [Some(target)] = bindings.as_slice() else {
                        return None;
                    };
                    let mut full = target.clone();
                    full.extend(segs[1..].iter().cloned());
                    return self.resolve(scope, &full, module, depth + 1);
                }
                // A glob into a module of this same file (`use super::*` in an
                // inline `mod tests`) is searched there. Any other glob may
                // supply the name, so the spelling is ambiguous.
                let mut found = None;
                for glob in &s.globs {
                    let target = self.resolve(scope, glob, true, depth + 1)?;
                    let inner = self.0.iter().position(|t| t.path == target)?;
                    if inner == scope {
                        continue;
                    }
                    if let Some(hit) = self.resolve(inner, segs, module, depth + 1)
                        && found.replace(hit).is_some()
                    {
                        return None;
                    }
                }
                found
            }
        }
    }
}

enum Owner {
    Class(usize),
    Impl(String),
}

/// Identity spelling of an identifier: `r#type` and `type` name the same item.
/// Measured names and ranges keep their source spelling for display.
fn unraw(name: &str) -> String {
    name.strip_prefix("r#").unwrap_or(name).to_owned()
}

/// The named path of a type, without generic arguments.
fn type_path(ex: &Extractor<'_, '_>, n: Node<'_>) -> Option<Vec<String>> {
    match n.kind() {
        "type_identifier" | "scoped_type_identifier" => segments(ex.text(n)),
        "generic_type" => type_path(ex, n.child_by_field_name("type")?),
        _ => None,
    }
}

impl Extractor<'_, '_> {
    /// Declared type and const parameter names of `n`, added to `scope`. `None`
    /// when a parameter list or the resulting scope exceeds the Decision 0004
    /// type-parameter limits (`members` per declaration and per scope); the
    /// caller reports that and omits what depends on the scope.
    fn type_params(
        &mut self,
        n: Node<'_>,
        scope: &BTreeSet<String>,
        depth: usize,
    ) -> Result<Option<BTreeSet<String>>> {
        let limit = self.b.limits.members;
        let Some(params) = n.child_by_field_name("type_parameters") else {
            return Ok(Some(scope.clone()));
        };
        let mut own = BTreeSet::new();
        for p in self.children(params, depth)? {
            if matches!(p.kind(), "type_parameter" | "const_parameter")
                && let Some(name) = p.child_by_field_name("name")
            {
                own.insert(unraw(self.text(name)));
                if own.len() > limit {
                    return Ok(None);
                }
            }
        }
        let inherited = scope.iter().filter(|name| *name != "Self").count();
        if self.exhausted || inherited.saturating_add(own.len()) > limit {
            return Ok(None);
        }
        self.b
            .reserve_text(own.iter().map(String::len).sum::<usize>() * 2);
        let mut all = scope.clone();
        all.extend(own);
        Ok(Some(all))
    }

    pub(super) fn rust_file(&mut self, root: Node<'_>) -> Result<()> {
        let (krate, module) = crate_module(&self.file.path);
        let mut scopes = Scopes(Vec::new(), std::cell::Cell::new(0));
        self.rust_collect(root, module, &mut scopes, 0)?;
        if !self.exhausted {
            self.rust_items(root, 0, &krate, &scopes, 0)?;
        }
        Ok(())
    }

    fn bind_use(
        &mut self,
        scopes: &mut Scopes,
        scope: usize,
        name: String,
        path: Option<Vec<String>>,
    ) {
        let charge = name.len() + path.as_ref().map_or(0, |p| p.iter().map(String::len).sum());
        if name.len() > self.b.limits.text || path.as_ref().is_some_and(|p| p.len() > 64) {
            self.b.registry_limit("Class binding text limit reached");
            scopes.0[scope].globs.push(vec![String::new()]);
            return;
        }
        self.b.reserve_text(charge);
        scopes.0[scope].uses.entry(name).or_default().push(path);
    }

    fn rust_use(
        &mut self,
        n: Node<'_>,
        prefix: &[String],
        scopes: &mut Scopes,
        scope: usize,
        depth: usize,
    ) -> Result<()> {
        if !self.tick(depth)? {
            return Ok(());
        }
        let join = |rest: Option<Vec<String>>| -> Option<Vec<String>> {
            let rest = rest?;
            if !prefix.is_empty() && rest.first().is_some_and(String::is_empty) {
                return None;
            }
            let mut full = prefix.to_vec();
            full.extend(rest);
            Some(full)
        };
        match n.kind() {
            "identifier" | "scoped_identifier" | "self" | "crate" | "super" => {
                let mut path = join(segments(self.text(n)));
                // `use a::{self}` binds `a`; `use a::b::self` is not valid Rust.
                if path
                    .as_ref()
                    .and_then(|p| p.last())
                    .is_some_and(|l| l == "self")
                {
                    path.as_mut().map(Vec::pop);
                }
                let Some(name) = path.as_ref().and_then(|p| p.last()).cloned() else {
                    scopes.0[scope].globs.push(vec![String::new()]);
                    return Ok(());
                };
                let path = path.filter(|p| !p.first().is_some_and(String::is_empty));
                self.bind_use(scopes, scope, name, path);
            }
            "use_as_clause" => {
                let (Some(path), Some(alias)) = (
                    n.child_by_field_name("path"),
                    n.child_by_field_name("alias"),
                ) else {
                    return Ok(());
                };
                let alias = unraw(self.text(alias));
                if alias == "_" {
                    return Ok(());
                }
                let mut path = join(segments(self.text(path)));
                if path
                    .as_ref()
                    .and_then(|p| p.last())
                    .is_some_and(|l| l == "self")
                {
                    path.as_mut().map(Vec::pop);
                }
                let path = path.filter(|p| !p.first().is_some_and(String::is_empty));
                self.bind_use(scopes, scope, alias, path);
            }
            "scoped_use_list" => {
                let inner = match n.child_by_field_name("path") {
                    Some(path) => join(segments(self.text(path))),
                    None => Some(prefix.to_vec()),
                };
                let Some(inner) = inner else {
                    scopes.0[scope].globs.push(vec![String::new()]);
                    return Ok(());
                };
                if let Some(list) = n.child_by_field_name("list") {
                    for c in self.children(list, depth + 1)? {
                        self.rust_use(c, &inner, scopes, scope, depth + 1)?;
                    }
                }
            }
            "use_list" => {
                for c in self.children(n, depth + 1)? {
                    self.rust_use(c, prefix, scopes, scope, depth + 1)?;
                }
            }
            "use_wildcard" => {
                let text = self.text(n).trim_end_matches('*').trim_end();
                let text = text.strip_suffix("::").unwrap_or(text);
                let glob = if text.is_empty() {
                    Some(prefix.to_vec())
                } else {
                    join(segments(text))
                };
                // An unprovable glob blocks every unbound name in this scope.
                scopes.0[scope]
                    .globs
                    .push(glob.unwrap_or_else(|| vec![String::new()]));
            }
            _ => {}
        }
        Ok(())
    }

    /// First pass: declared names, modules and `use` bindings of every module scope.
    fn rust_collect(
        &mut self,
        n: Node<'_>,
        path: Vec<String>,
        scopes: &mut Scopes,
        depth: usize,
    ) -> Result<usize> {
        let scope = scopes.0.len();
        scopes.0.push(ModScope {
            path: path.clone(),
            ..ModScope::default()
        });
        for item in self.children(n, depth)? {
            let name = item
                .child_by_field_name("name")
                .map(|name| unraw(self.text(name)));
            match (item.kind(), name) {
                ("struct_item" | "enum_item" | "union_item" | "trait_item", Some(name)) => {
                    self.b.reserve_text(name.len());
                    *scopes.0[scope].types.entry(name).or_default() += 1;
                }
                ("type_item", Some(name)) => {
                    scopes.0[scope].aliases.insert(name);
                }
                ("mod_item", Some(name)) => {
                    scopes.0[scope].mods.insert(name.clone());
                    if let Some(body) = item.child_by_field_name("body") {
                        let mut child = path.clone();
                        child.push(name);
                        let index = self.rust_collect(body, child, scopes, depth + 1)?;
                        scopes.0[scope].children.insert(item.start_byte(), index);
                    }
                }
                ("use_declaration", _) => {
                    if let Some(argument) = item.child_by_field_name("argument") {
                        self.rust_use(argument, &[], scopes, scope, depth + 1)?;
                    }
                }
                ("extern_crate_declaration", _) => {
                    let alias = item
                        .child_by_field_name("alias")
                        .or_else(|| item.child_by_field_name("name"));
                    if let Some(alias) = alias {
                        let alias = unraw(self.text(alias));
                        self.bind_use(scopes, scope, alias, None);
                    }
                }
                _ => {}
            }
        }
        Ok(scope)
    }

    /// Second pass: boxes, members and impl contributions, in source order.
    fn rust_items(
        &mut self,
        n: Node<'_>,
        scope: usize,
        krate: &str,
        scopes: &Scopes,
        depth: usize,
    ) -> Result<()> {
        let mut derives: Vec<(Node<'_>, String)> = Vec::new();
        for item in self.children(n, depth)? {
            if self.exhausted {
                break;
            }
            match item.kind() {
                "attribute_item" => {
                    self.derive_names(item, &mut derives);
                    continue;
                }
                "line_comment" | "block_comment" => continue,
                "struct_item" | "enum_item" | "union_item" | "trait_item" => {
                    self.rust_class(item, scope, krate, scopes, &derives, depth + 1)?;
                }
                "impl_item" => self.rust_impl(item, scope, krate, scopes, depth + 1)?,
                "mod_item" => {
                    if let (Some(body), Some(child)) = (
                        item.child_by_field_name("body"),
                        scopes.0[scope].children.get(&item.start_byte()),
                    ) {
                        self.rust_items(body, *child, krate, scopes, depth + 1)?;
                    }
                }
                _ => {}
            }
            derives.clear();
        }
        Ok(())
    }

    /// `#[derive(A, b::C)]` names. Derived impls are generated, so they are only
    /// recorded as terminal `derives` hints, never members or `implements`.
    fn derive_names<'t>(&self, item: Node<'t>, out: &mut Vec<(Node<'t>, String)>) {
        let text = self.text(item);
        let Some(inner) = text
            .strip_prefix("#[")
            .and_then(|t| t.strip_suffix(']'))
            .map(str::trim)
            .and_then(|t| t.strip_prefix("derive"))
            .map(str::trim_start)
            .and_then(|t| t.strip_prefix('('))
            .and_then(|t| t.strip_suffix(')'))
        else {
            return;
        };
        for name in inner.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if segments(name).is_some() {
                out.push((item, name.to_owned()));
            }
        }
    }

    fn rust_class(
        &mut self,
        n: Node<'_>,
        scope: usize,
        krate: &str,
        scopes: &Scopes,
        derives: &[(Node<'_>, String)],
        depth: usize,
    ) -> Result<()> {
        if !self.tick(depth)? {
            return Ok(());
        }
        if self.classes >= self.b.limits.file_classes {
            self.exhausted = true;
            self.b.registry_limit(&format!(
                "{}: class declaration limit reached (1000/file, 20000/catalog)",
                self.file.path
            ));
            return Ok(());
        }
        let Some(symbol) = self.b.symbol(self.file, n, SymbolKind::Class) else {
            self.b.registry_limit(&format!(
                "{}: declaration has no unique measured class symbol; skipped",
                self.file.path
            ));
            return Ok(());
        };
        let Some(name) = n.child_by_field_name("name").map(|n| unraw(self.text(n))) else {
            return Ok(());
        };
        let qualified = qualified(krate, &scopes.0[scope].path, &name);
        self.b.reserve_text(
            symbol.id.len() + symbol.name.len() + symbol.path.len() + qualified.len(),
        );
        if qualified.len() > self.b.limits.text {
            self.b
                .registry_limit("Class qualified name exceeds 2048 bytes");
            return Ok(());
        }
        let Some(blocked) = self.type_params(n, &BTreeSet::from(["Self".to_owned()]), depth + 1)?
        else {
            self.b.registry_limit(&format!(
                "Class type parameter limit reached ({}/declaration)",
                self.b.limits.members
            ));
            return Ok(());
        };
        let index = self.b.catalog.classes.len();
        self.classes += 1;
        self.b.catalog.classes.push(ClassDefinition {
            symbol,
            qualified_name: qualified,
            language: "rust".into(),
            declaration_kind: n.kind().trim_end_matches("_item").into(),
            fields: vec![],
            methods: vec![],
            truncated: false,
        });
        let owner = Owner::Class(index);
        for (attribute, derived) in derives {
            self.rust_reference(&owner, *attribute, derived, "derives")?;
        }
        let Some(body) = n.child_by_field_name("body") else {
            return Ok(());
        };
        match n.kind() {
            "trait_item" => {
                if let Some(bounds) = n.child_by_field_name("bounds") {
                    for bound in self.children(bounds, depth + 1)? {
                        if let Some(path) = type_path(self, bound) {
                            self.rust_reference(&owner, bound, &path.join("::"), "extends")?;
                        }
                    }
                }
                for item in self.children(body, depth + 1)? {
                    if matches!(item.kind(), "function_item" | "function_signature_item") {
                        self.rust_method(item, &owner, &blocked, depth + 1)?;
                    }
                }
            }
            "enum_item" => {
                for variant in self.children(body, depth + 1)? {
                    if variant.kind() != "enum_variant" {
                        continue;
                    }
                    let Some(name) = variant.child_by_field_name("name") else {
                        continue;
                    };
                    let payload = variant.child_by_field_name("body");
                    let hint = payload.map(|p| self.text(p).to_owned());
                    self.rust_member(
                        &owner,
                        variant,
                        self.text(name).to_owned(),
                        hint,
                        None,
                        false,
                    )?;
                    if let Some(payload) = payload {
                        self.rust_fields(payload, &owner, &blocked, depth + 1, false)?;
                    }
                }
            }
            _ => self.rust_fields(body, &owner, &blocked, depth + 1, true)?,
        }
        if self.exhausted {
            self.b.catalog.classes[index].truncated = true;
        }
        Ok(())
    }

    /// Named or tuple fields; `members` is false for enum variant payloads, whose
    /// types are relations of the enum but whose fields are not enum members.
    fn rust_fields(
        &mut self,
        list: Node<'_>,
        owner: &Owner,
        blocked: &BTreeSet<String>,
        depth: usize,
        members: bool,
    ) -> Result<()> {
        let mut ordinal = 0usize;
        for field in self.children(list, depth)? {
            match field.kind() {
                "field_declaration" => {
                    let (Some(name), Some(ty)) = (
                        field.child_by_field_name("name"),
                        field.child_by_field_name("type"),
                    ) else {
                        continue;
                    };
                    if members {
                        let hint = Some(self.text(ty).to_owned());
                        self.rust_member(
                            owner,
                            field,
                            self.text(name).to_owned(),
                            hint,
                            None,
                            false,
                        )?;
                    }
                    self.rust_types(ty, owner, "field", blocked, depth + 1)?;
                }
                "attribute_item" | "visibility_modifier" => {}
                _ if list.kind() == "ordered_field_declaration_list" => {
                    if members {
                        let hint = Some(self.text(field).to_owned());
                        self.rust_member(owner, field, ordinal.to_string(), hint, None, false)?;
                    }
                    ordinal += 1;
                    self.rust_types(field, owner, "field", blocked, depth + 1)?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn rust_impl(
        &mut self,
        n: Node<'_>,
        scope: usize,
        krate: &str,
        scopes: &Scopes,
        depth: usize,
    ) -> Result<()> {
        if !self.tick(depth)? {
            return Ok(());
        }
        let mut cursor = n.walk();
        if n.children(&mut cursor).any(|c| c.kind() == "!") {
            return Ok(()); // A negative impl declares nothing.
        }
        let Some(blocked) = self.type_params(n, &BTreeSet::from(["Self".to_owned()]), depth + 1)?
        else {
            // The impl cannot be attributed without its parameter scope.
            self.b.registry_limit(&format!(
                "{}: impl type parameter limit reached ({}/declaration); impl not attributed",
                self.file.path, self.b.limits.members
            ));
            return Ok(());
        };
        let segs = n
            .child_by_field_name("type")
            .and_then(|ty| type_path(self, ty))
            .filter(|segs| !(segs.len() == 1 && blocked.contains(&segs[0])))
            .filter(|segs| !segs.first().is_some_and(|s| blocked.contains(s)));
        let Some(segs) = segs else {
            // `impl<T: X> Y for T`, `impl Y for &T`, `dyn` or tuple self types.
            self.b.notice(&format!(
                "{}: blanket or non-path impl not attributed to a class",
                self.file.path
            ));
            return Ok(());
        };
        let resolved = scopes.resolve(scope, &segs, false, 0);
        if scopes.1.get() > RESOLVE_STEPS {
            self.b.limit(&format!(
                "{}: impl path resolution work limit reached ({RESOLVE_STEPS}); later impls not attributed",
                self.file.path
            ));
        }
        let Some(path) = resolved else {
            return Ok(()); // External, re-exported, aliased or ambiguous: no guess.
        };
        let Some((name, module)) = path.split_last() else {
            return Ok(());
        };
        let owner = Owner::Impl(qualified(krate, module, name));
        if let Some(t) = n.child_by_field_name("trait") {
            if let Some(path) = type_path(self, t) {
                self.rust_reference(&owner, t, &path.join("::"), "implements")?;
            }
            // Trait methods belong to the trait's box, not the implementing type's.
            return Ok(());
        }
        if let Some(body) = n.child_by_field_name("body") {
            for item in self.children(body, depth + 1)? {
                if item.kind() == "function_item" {
                    self.rust_method(item, &owner, &blocked, depth + 1)?;
                }
            }
        }
        Ok(())
    }

    fn rust_method(
        &mut self,
        n: Node<'_>,
        owner: &Owner,
        outer: &BTreeSet<String>,
        depth: usize,
    ) -> Result<()> {
        let Some(name) = n.child_by_field_name("name") else {
            return Ok(());
        };
        let ret = n.child_by_field_name("return_type");
        let symbol = self
            .b
            .symbol(self.file, n, SymbolKind::Method)
            .or_else(|| self.b.symbol(self.file, n, SymbolKind::Function))
            .map(|s| s.id);
        let hint = ret.map(|r| self.text(r).to_owned());
        self.rust_member(owner, n, self.text(name).to_owned(), hint, symbol, true)?;
        let Some(blocked) = self.type_params(n, outer, depth + 1)? else {
            match owner {
                Owner::Class(index) => self.b.catalog.classes[*index].truncated = true,
                // Composition marks the target class truncated.
                Owner::Impl(target) => self.b.impls.push(ImplItem {
                    target: target.clone(),
                    records: 0,
                    text: 0,
                    value: DetailValue::ScopeText,
                }),
            }
            self.b.limit(&format!(
                "Method type parameter detail limit reached ({}); method references omitted",
                self.b.limits.members
            ));
            return Ok(());
        };
        if let Some(ret) = ret {
            self.rust_types(ret, owner, "returns", &blocked, depth + 1)?;
        }
        if let Some(params) = n.child_by_field_name("parameters") {
            for p in self.children(params, depth + 1)? {
                if p.kind() == "parameter"
                    && let Some(ty) = p.child_by_field_name("type")
                {
                    self.rust_types(ty, owner, "parameter", &blocked, depth + 2)?;
                }
            }
        }
        // Deliberately never inspect the function body.
        Ok(())
    }

    fn rust_detail(&mut self, owner: &Owner, depth: usize) -> Result<bool> {
        check(self.b.cancel)?;
        if self.detail_visits >= self.b.limits.visits || depth >= self.b.limits.depth {
            if let Owner::Class(index) = owner {
                self.b.catalog.classes[*index].truncated = true;
            }
            self.b.limit(if depth >= self.b.limits.depth {
                "Class type detail depth limit reached (64); declaration discovery continues"
            } else {
                "Class detail limit reached (250000 records / 64 MiB text / 100000 visits per file); declaration discovery continues"
            });
            return Ok(false);
        }
        self.detail_visits += 1;
        Ok(true)
    }

    fn rust_types(
        &mut self,
        n: Node<'_>,
        owner: &Owner,
        kind: &str,
        blocked: &BTreeSet<String>,
        depth: usize,
    ) -> Result<()> {
        if !self.rust_detail(owner, depth)? {
            return Ok(());
        }
        match n.kind() {
            "type_identifier" | "scoped_type_identifier" => {
                if let Some(path) = segments(self.text(n))
                    && !path
                        .first()
                        .is_some_and(|s| s.is_empty() || blocked.contains(s))
                {
                    self.rust_reference(owner, n, &path.join("::"), kind)?;
                }
            }
            "generic_type" => {
                if let Some(base) = n.child_by_field_name("type") {
                    self.rust_types(base, owner, kind, blocked, depth + 1)?;
                }
                if let Some(args) = n.child_by_field_name("type_arguments") {
                    self.rust_types(args, owner, kind, blocked, depth + 1)?;
                }
            }
            "type_arguments" | "tuple_type" | "bounded_type" => {
                let mut cursor = n.walk();
                let children: Vec<_> = n.named_children(&mut cursor).collect();
                for c in children {
                    self.rust_types(c, owner, kind, blocked, depth + 1)?;
                }
            }
            "type_binding" | "reference_type" | "pointer_type" => {
                if let Some(t) = n.child_by_field_name("type") {
                    self.rust_types(t, owner, kind, blocked, depth + 1)?;
                }
            }
            "array_type" => {
                if let Some(t) = n.child_by_field_name("element") {
                    self.rust_types(t, owner, kind, blocked, depth + 1)?;
                }
            }
            "dynamic_type" | "abstract_type" => {
                if let Some(t) = n.child_by_field_name("trait") {
                    self.rust_types(t, owner, kind, blocked, depth + 1)?;
                }
            }
            // Primitive, unit, never, function, qualified and macro types name no class.
            _ => {}
        }
        Ok(())
    }

    fn rust_member(
        &mut self,
        owner: &Owner,
        n: Node<'_>,
        name: String,
        hint: Option<String>,
        symbol_id: Option<String>,
        method: bool,
    ) -> Result<()> {
        if !self.rust_detail(owner, 0)? {
            return Ok(());
        }
        let hint = hint.map(|h| h.split_whitespace().collect::<Vec<_>>().join(" "));
        if name.len() > self.b.limits.text
            || hint.as_ref().is_some_and(|h| h.len() > self.b.limits.text)
        {
            if let Owner::Class(index) = owner {
                self.b.catalog.classes[*index].truncated = true;
            }
            self.b.limit("Class member text exceeds 2048 bytes");
            return Ok(());
        }
        let member = ClassMember {
            name,
            type_hint: hint,
            symbol_id,
            path: self.file.path.clone(),
            range: range(n),
        };
        let text = member.name.len()
            + member.type_hint.as_ref().map_or(0, String::len)
            + member.symbol_id.as_ref().map_or(0, String::len)
            + member.path.len();
        self.b.reserve_detail_text(text);
        let value = if method {
            DetailValue::Method(member.clone())
        } else {
            DetailValue::Field(member.clone())
        };
        match owner {
            Owner::Class(index) => {
                let c = &mut self.b.catalog.classes[*index];
                if c.fields.len() + c.methods.len() >= self.b.limits.members {
                    c.truncated = true;
                    self.b.limit(&format!(
                        "{}: class member limit reached (256/class)",
                        self.file.path
                    ));
                    return Ok(());
                }
                if method {
                    c.methods.push(member);
                } else {
                    c.fields.push(member);
                }
                self.b.items.push(DetailItem {
                    class_index: *index,
                    records: 1,
                    text,
                    value,
                });
            }
            Owner::Impl(target) => self.b.impls.push(ImplItem {
                target: target.clone(),
                records: 1,
                text: text + target.len(),
                value,
            }),
        }
        self.b.records += 1;
        Ok(())
    }

    fn rust_reference(&mut self, owner: &Owner, n: Node<'_>, name: &str, kind: &str) -> Result<()> {
        if !self.rust_detail(owner, 0)? {
            return Ok(());
        }
        if self.refs >= self.b.limits.file_refs || name.len() > self.b.limits.text {
            if let Owner::Class(index) = owner {
                self.b.catalog.classes[*index].truncated = true;
            }
            self.b.limit(&format!(
                "{}: class reference limit reached (8192/file, 2048 bytes/name)",
                self.file.path
            ));
            return Ok(());
        }
        let owner_id = match owner {
            Owner::Class(index) => self.b.catalog.classes[*index].symbol.id.clone(),
            Owner::Impl(_) => String::new(),
        };
        let relation = ClassRelation {
            id: format!(
                "class-ref:{owner_id}:{}:{}:{kind}:{name}",
                n.start_byte(),
                n.end_byte()
            ),
            owner: owner_id,
            target: None,
            type_name: name.into(),
            kind: kind.into(),
            path: self.file.path.clone(),
            range: range(n),
            candidate_ids: vec![],
            match_kind: "unmatched".into(),
        };
        let text = relation.id.len()
            + relation.owner.len()
            + relation.type_name.len()
            + relation.path.len();
        self.b.reserve_detail_text(text);
        match owner {
            Owner::Class(index) => {
                self.b.items.push(DetailItem {
                    class_index: *index,
                    records: 1,
                    text,
                    value: DetailValue::Relation(relation.clone()),
                });
                self.b.pending.push(Pending { relation });
            }
            Owner::Impl(target) => self.b.impls.push(ImplItem {
                target: target.clone(),
                records: 1,
                text: text + target.len(),
                value: DetailValue::Relation(relation),
            }),
        }
        self.refs += 1;
        self.b.records += 1;
        Ok(())
    }
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn conventional_cargo_paths_name_crates_and_modules() {
        let m = |p: &str| crate_module(p);
        let v = |s: &[&str]| s.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(m("src/lib.rs"), (String::new(), v(&[])));
        assert_eq!(m("src/main.rs"), (String::new(), v(&[])));
        assert_eq!(m("src/store.rs"), (String::new(), v(&["store"])));
        assert_eq!(m("src/store/mod.rs"), (String::new(), v(&["store"])));
        assert_eq!(
            m("src/store/topology.rs"),
            (String::new(), v(&["store", "topology"]))
        );
        assert_eq!(
            m("crates/a/src/x/y.rs"),
            ("crates/a".into(), v(&["x", "y"]))
        );
        assert_eq!(m("src/bin/tool.rs"), ("src/bin/tool.rs".into(), v(&[])));
        assert_eq!(
            m("src/bin/tool/main.rs"),
            ("src/bin/tool/main.rs".into(), v(&[]))
        );
        assert_eq!(
            m("src/bin/tool/io.rs"),
            ("src/bin/tool/main.rs".into(), v(&["io"]))
        );
        assert_eq!(m("tests/it.rs"), ("tests/it.rs".into(), v(&[])));
        assert_eq!(m("build.rs"), ("build.rs".into(), v(&[])));
        assert_eq!(qualified("", &v(&["store"]), "Store"), "store::Store");
        assert_eq!(
            qualified("tests/it.rs", &[], "Fixture"),
            "tests/it.rs::Fixture"
        );
        assert_eq!(qualified("crates/a", &v(&["x"]), "T"), "crates/a::x::T");
    }

    #[test]
    fn path_segments_reject_generics_and_mark_extern_paths() {
        assert_eq!(
            segments("a :: b::C"),
            Some(vec!["a".into(), "b".into(), "C".into()])
        );
        assert_eq!(
            segments("::std::fmt"),
            Some(vec![String::new(), "std".into(), "fmt".into()])
        );
        assert_eq!(segments("r#type::X"), Some(vec!["type".into(), "X".into()]));
        assert_eq!(segments("a::<T>::B"), None);
        assert_eq!(segments("Vec<T>"), None);
    }
}
