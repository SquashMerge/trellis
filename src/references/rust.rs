//! Bounded, captured-syntax Rust name and module proofs, not compiler bindings.
//! Foreign-block items and derive-generated sibling names are not modeled.
use super::*;

type RouteKey = (usize, String);
type Route = BTreeSet<RouteKey>;

#[derive(Clone, Copy)]
struct ResolveContext<'a, 'b> {
    packages: &'a external::RustPackages,
    docs: &'a [&'b DocumentInput],
    waive_external: bool,
}
#[derive(Default)]
struct ExportTrace {
    consulted: Route,
    cycle: bool,
}
struct ExportMemo {
    names: std::sync::Arc<Names>,
    consulted: Route,
}
struct ScopeIndex {
    ranges: Vec<(usize, usize, usize)>,
    max_ends: Vec<usize>,
}
impl ScopeIndex {
    fn build(
        &mut self,
        left: usize,
        right: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        if left == right {
            return Ok(0);
        }
        owner_step(work, check)?;
        let mid = left + (right - left) / 2;
        let end = self.ranges[mid]
            .2
            .max(self.build(left, mid, work, check)?)
            .max(self.build(mid + 1, right, work, check)?);
        self.max_ends[mid] = end;
        Ok(end)
    }
    fn enclosing(
        &self,
        start: usize,
        end: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<usize>> {
        let mut found: Option<(usize, usize)> = None;
        let mut pending = vec![(0, self.ranges.len())];
        while let Some((left, right)) = pending.pop() {
            if left == right {
                continue;
            }
            owner_step(work, check)?;
            let mid = left + (right - left) / 2;
            if self.ranges[left].1 > start || self.max_ends[mid] < end {
                continue;
            }
            let (id, scope_start, scope_end) = self.ranges[mid];
            if scope_start <= start && scope_end >= end {
                let candidate = (scope_end - scope_start, std::cmp::Reverse(id));
                if found.is_none_or(|(size, old)| candidate < (size, std::cmp::Reverse(old))) {
                    found = Some((scope_end - scope_start, id));
                }
            }
            pending.push((mid + 1, right));
            pending.push((left, mid));
        }
        Ok(found.map(|(_, id)| id))
    }
}
type Name = (u8, String);
const TYPE: u8 = 0;
const VALUE: u8 = 1;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DeclarationId {
    document: usize,
    start: usize,
    end: usize,
    sid: String,
}
#[derive(Clone)]
struct Candidate {
    declaration: DeclarationId,
    module: Option<usize>,
    follow: usize,
    proof: BTreeSet<usize>,
}
#[derive(Clone, Default)]
struct Bucket {
    candidates: BTreeMap<DeclarationId, Candidate>,
    unknown: bool,
}
#[derive(Default)]
struct Names {
    names: BTreeMap<Name, Bucket>,
    unknown_set: bool,
}
#[derive(Clone)]
struct Import {
    start: usize,
    end: usize,
    path: Vec<String>,
    alias: Option<String>,
    star: bool,
    public: bool,
    uncertain: bool,
}
#[derive(Clone)]
struct Binding {
    candidate: Option<Candidate>,
    public: bool,
    uncertain: bool,
}
struct Scope {
    document: usize,
    start: usize,
    end: usize,
    parent: Option<usize>,
    module: usize,
    directory: String,
    explicit: BTreeMap<Name, Vec<Binding>>,
    imports: Vec<Import>,
    opaque: bool,
}
struct ModuleEdge {
    scope: usize,
    name: String,
    start: usize,
    end: usize,
    child: Option<usize>,
}
pub(super) struct Catalog {
    scopes: Vec<Scope>,
    scope_indexes: BTreeMap<usize, ScopeIndex>,
    export_memo: BTreeMap<(usize, usize, bool), std::sync::Arc<ExportMemo>>,
    export_traces: Vec<ExportTrace>,
    files: BTreeMap<usize, usize>,
    file_declarations: BTreeMap<usize, DeclarationId>,
    roots: BTreeMap<usize, BTreeSet<usize>>,
    ambiguous: BTreeSet<usize>,
    pub(super) work: Work,
}

impl Catalog {
    pub(super) fn new(
        docs: &[&DocumentInput],
        trees: &[Tree],
        documents: &BTreeMap<DocumentKey, usize>,
        work: Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut result = Self {
            scopes: Vec::new(),
            scope_indexes: BTreeMap::new(),
            export_memo: BTreeMap::new(),
            export_traces: Vec::new(),
            files: BTreeMap::new(),
            file_declarations: BTreeMap::new(),
            roots: BTreeMap::new(),
            ambiguous: BTreeSet::new(),
            work,
        };
        let mut declarations = BTreeMap::new();
        for (document, doc) in docs.iter().enumerate() {
            if doc.key.language != "rust" {
                continue;
            }
            for declaration in &doc.declarations {
                result.step(check)?;
                // The file module can span exactly the same bytes as its sole item.
                if declaration.kind == "module" && declaration.ancestors.is_empty() {
                    continue;
                }
                declarations
                    .entry((document, declaration.range.start, declaration.range.end))
                    .or_insert_with(Vec::new)
                    .push(declaration);
            }
            if let Some(declaration) = doc
                .declarations
                .iter()
                .find(|d| d.kind == "module" && d.ancestors.is_empty())
            {
                result.file_declarations.insert(
                    document,
                    DeclarationId {
                        document,
                        start: declaration.range.start,
                        end: declaration.range.end,
                        sid: declaration.syntax_id.clone(),
                    },
                );
            }
            let path = &doc.key.path;
            let directory = if matches!(path.as_str(), "src/lib.rs" | "src/main.rs") {
                "src".to_owned()
            } else if let Some(prefix) = path.strip_suffix("/mod.rs") {
                prefix.to_owned()
            } else {
                path.strip_suffix(".rs").unwrap_or(path).to_owned()
            };
            let scope = result.add_scope(
                document,
                trees[document].root_node(),
                None,
                directory,
                true,
                check,
            )?;
            result.files.insert(document, scope);
        }
        let mut edges = Vec::new();
        for (&document, &scope) in &result.files.clone() {
            result.scan(
                docs,
                trees[document].root_node(),
                scope,
                &declarations,
                &mut edges,
                0,
                check,
            )?;
        }
        for edge in &mut edges {
            result.step(check)?;
            if edge.child.is_none() {
                let scope = &result.scopes[edge.scope];
                let doc = docs[scope.document];
                let prefix = format!("{}/{}", scope.directory, edge.name);
                let candidates = find(
                    doc,
                    &[format!("{prefix}.rs"), format!("{prefix}/mod.rs")],
                    documents,
                );
                if let [document] = candidates.as_slice() {
                    edge.child = result.files.get(document).copied();
                }
            }
            if let Some(child) = edge.child {
                let key = (TYPE, edge.name.clone());
                if let Some(bindings) = result.scopes[edge.scope].explicit.get_mut(&key) {
                    for binding in bindings {
                        if let Some(candidate) = &mut binding.candidate
                            && candidate.declaration.start == edge.start
                            && candidate.declaration.end == edge.end
                        {
                            candidate.module = Some(child);
                        }
                    }
                }
            }
        }
        let mut parents: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for edge in &edges {
            result.step(check)?;
            if let Some(child) = edge.child {
                parents.entry(child).or_default().insert(edge.scope);
            }
        }
        for (child, parents) in parents {
            if parents.len() == 1 {
                result.scopes[child].parent = parents.first().copied();
            } else {
                result.ambiguous.insert(child);
            }
        }
        // A file is owned only by a unique, witnessed chain from a conventional root.
        for (&document, &scope) in &result.files.clone() {
            if !matches!(
                docs[document].key.path.as_str(),
                "src/lib.rs" | "src/main.rs"
            ) {
                continue;
            }
            let mut pending = vec![(scope, BTreeSet::new())];
            while let Some((current, mut route)) = pending.pop() {
                result.step(check)?;
                if !route.insert(current) {
                    continue;
                }
                result.roots.entry(current).or_default().insert(scope);
                for edge in &edges {
                    result.step(check)?;
                    if edge.scope != current {
                        continue;
                    }
                    let bindings = result.scopes[current]
                        .explicit
                        .get(&(TYPE, edge.name.clone()));
                    if bindings.is_some_and(|b| b.len() == 1)
                        && let Some(child) = edge.child
                    {
                        pending.push((child, route.clone()));
                    }
                }
            }
        }
        for id in 0..result.scopes.len() {
            result.step(check)?;
            let scope = &result.scopes[id];
            result
                .scope_indexes
                .entry(scope.document)
                .or_insert_with(|| ScopeIndex {
                    ranges: Vec::new(),
                    max_ends: Vec::new(),
                })
                .ranges
                .push((id, scope.start, scope.end));
        }
        for index in result.scope_indexes.values_mut() {
            index.ranges.sort_by_key(|&(id, start, _)| (start, id));
            check()?;
            index.max_ends.resize(index.ranges.len(), 0);
            index.build(0, index.ranges.len(), &mut result.work, check)?;
        }
        Ok(result)
    }
    fn step(&mut self, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
        check()?;
        ensure!(
            self.work.charge(1),
            "bounded_work: reference Rust resolution"
        );
        Ok(())
    }
    fn charge(&mut self, count: usize, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
        for _ in 0..count {
            self.step(check)?;
        }
        Ok(())
    }
    fn scope_imports(
        &mut self,
        scope: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Vec<Import>> {
        self.charge(self.scopes[scope].imports.len(), check)?;
        Ok(self.scopes[scope].imports.clone())
    }
    fn add_scope(
        &mut self,
        document: usize,
        node: Node<'_>,
        parent: Option<usize>,
        directory: String,
        module: bool,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<usize> {
        self.step(check)?;
        let id = self.scopes.len();
        let module = if module {
            id
        } else {
            self.scopes[parent.expect("block parent")].module
        };
        self.scopes.push(Scope {
            document,
            start: node.start_byte(),
            end: node.end_byte(),
            parent,
            module,
            directory,
            explicit: BTreeMap::new(),
            imports: Vec::new(),
            opaque: false,
        });
        Ok(id)
    }
    #[allow(clippy::too_many_arguments)] // Recursive scan keeps its captured inputs and budget explicit.
    fn scan<'a>(
        &mut self,
        docs: &[&DocumentInput],
        node: Node<'a>,
        scope: usize,
        declarations: &BTreeMap<(usize, usize, usize), Vec<&Declaration>>,
        edges: &mut Vec<ModuleEdge>,
        depth: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        self.step(check)?;
        ensure!(depth <= DEPTH, "bounded_work: reference AST");
        let document = self.scopes[scope].document;
        let doc = docs[document];
        let mut cursor = node.walk();
        let mut uncertain = false;
        let mut scope_uncertain = false;
        let mut custom_path = false;
        for child in node.named_children(&mut cursor) {
            self.step(check)?;
            if matches!(child.kind(), "line_comment" | "block_comment") {
                continue;
            }
            if matches!(child.kind(), "attribute_item" | "inner_attribute_item") {
                let attribute: String = text(doc, child)
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .collect();
                if attribute.starts_with("#![cfg(") || attribute.starts_with("#![cfg_attr(") {
                    scope_uncertain = true;
                } else if attribute.starts_with("#[cfg(")
                    || attribute.starts_with("#[cfg_attr(")
                    || attribute == "#[test]"
                {
                    uncertain = true;
                } else if attribute.starts_with("#[path=") {
                    custom_path = true;
                } else if ![
                    "#[derive(",
                    "#[allow(",
                    "#[warn(",
                    "#[deny(",
                    "#[forbid(",
                    "#[doc",
                    "#[test]",
                    "#[inline",
                    "#[repr(",
                    "#[must_use",
                    "#[cold]",
                    "#[no_mangle]",
                ]
                .iter()
                .any(|a| attribute.starts_with(a))
                {
                    self.scopes[scope].opaque = true;
                    if child.kind() == "inner_attribute_item" {
                        scope_uncertain = true;
                    } else {
                        uncertain = true;
                    }
                }
                continue;
            }
            let item_uncertain = uncertain || scope_uncertain;
            uncertain = false;
            let item_custom = custom_path;
            custom_path = false;
            if child.kind() == "extern_crate_declaration" {
                if let Some(name) = child
                    .child_by_field_name("alias")
                    .or_else(|| child.child_by_field_name("name"))
                {
                    self.scopes[scope]
                        .explicit
                        .entry((TYPE, text(doc, name).to_owned()))
                        .or_default()
                        .push(Binding {
                            candidate: None,
                            public: visibility(doc, child).is_some(),
                            uncertain: true,
                        });
                }
                continue;
            }
            if child.kind() == "use_declaration" {
                let public = visibility(doc, child).is_some();
                let restricted = visibility(doc, child).is_some_and(|v| v != "pub");
                if let Some(argument) = child.child_by_field_name("argument") {
                    let imports = imports(
                        doc,
                        argument,
                        public,
                        item_uncertain || restricted,
                        &mut self.work,
                        check,
                    )?;
                    self.scopes[scope].imports.extend(imports);
                }
                continue;
            }
            if child.kind() == "macro_invocation"
                || (child.kind() == "expression_statement" && text(doc, child).contains('!'))
            {
                self.scopes[scope].opaque = true;
                continue;
            }
            if opaque(child) {
                continue;
            }
            let namespaces: &[u8] = match child.kind() {
                "mod_item" | "type_item" | "trait_item" | "union_item" | "enum_item" => &[TYPE],
                "function_item" | "const_item" | "static_item" => &[VALUE],
                "struct_item" => {
                    if child
                        .child_by_field_name("body")
                        .is_some_and(|b| b.kind() == "field_declaration_list")
                    {
                        &[TYPE]
                    } else {
                        &[TYPE, VALUE]
                    }
                }
                _ => &[],
            };
            if !namespaces.is_empty()
                && let Some(name) = child.child_by_field_name("name")
            {
                let name = text(doc, name).to_owned();
                let declaration = declarations
                    .get(&(document, child.start_byte(), child.end_byte()))
                    .and_then(|d| (d.len() == 1).then(|| d[0]));
                let candidate = declaration.map(|d| Candidate {
                    declaration: DeclarationId {
                        document,
                        start: d.range.start,
                        end: d.range.end,
                        sid: d.syntax_id.clone(),
                    },
                    module: None,
                    follow: 0,
                    proof: BTreeSet::from([document]),
                });
                let public = visibility(doc, child).is_some();
                let restricted = visibility(doc, child).is_some_and(|v| v != "pub");
                for &namespace in namespaces {
                    self.step(check)?;
                    self.scopes[scope]
                        .explicit
                        .entry((namespace, name.clone()))
                        .or_default()
                        .push(Binding {
                            candidate: candidate.clone(),
                            public,
                            uncertain: item_uncertain
                                || restricted
                                || item_custom
                                || declaration.is_some_and(|d| d.role == DeclarationRole::Test),
                        });
                }
                if child.kind() == "mod_item" {
                    let directory = format!("{}/{}", self.scopes[scope].directory, name);
                    let child_scope = if let Some(body) = child.child_by_field_name("body") {
                        let child_scope =
                            self.add_scope(document, body, Some(scope), directory, true, check)?;
                        self.scan(
                            docs,
                            body,
                            child_scope,
                            declarations,
                            edges,
                            depth + 1,
                            check,
                        )?;
                        Some(child_scope)
                    } else {
                        None
                    };
                    if !item_custom {
                        edges.push(ModuleEdge {
                            scope,
                            name,
                            start: child.start_byte(),
                            end: child.end_byte(),
                            child: child_scope,
                        });
                    }
                    continue;
                }
            }
            if matches!(
                child.kind(),
                "impl_item" | "trait_item" | "enum_item" | "struct_item" | "union_item"
            ) {
                // Members are not module exports; their nested bodies still have lexical scopes.
                self.scan_bodies(docs, child, scope, declarations, edges, depth + 1, check)?;
            } else if child.kind() == "block" {
                let block = self.add_scope(
                    document,
                    child,
                    Some(scope),
                    self.scopes[scope].directory.clone(),
                    false,
                    check,
                )?;
                self.bind_parameters(docs, child, block, check)?;
                self.scan(docs, child, block, declarations, edges, depth + 1, check)?;
            } else {
                self.scan(docs, child, scope, declarations, edges, depth + 1, check)?;
            }
        }
        Ok(())
    }
    fn bind_parameters(
        &mut self,
        docs: &[&DocumentInput],
        body: Node<'_>,
        scope: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let doc = docs[self.scopes[scope].document];
        let mut ancestor = body.parent();
        let mut depth = 0;
        while let Some(node) = ancestor {
            self.step(check)?;
            depth += 1;
            ensure!(depth <= DEPTH, "bounded_work: reference AST");
            if node.kind() == "mod_item" || node.kind() == "source_file" {
                break;
            }
            if let Some(parameters) = node.child_by_field_name("type_parameters") {
                let mut cursor = parameters.walk();
                for parameter in parameters.named_children(&mut cursor) {
                    self.step(check)?;
                    if let Some(name) = parameter.child_by_field_name("name") {
                        let namespace = if parameter.kind() == "const_parameter" {
                            VALUE
                        } else {
                            TYPE
                        };
                        self.scopes[scope]
                            .explicit
                            .entry((namespace, text(doc, name).to_owned()))
                            .or_default()
                            .push(Binding {
                                candidate: None,
                                public: false,
                                uncertain: true,
                            });
                    }
                }
            }
            ancestor = node.parent();
        }
        Ok(())
    }
    #[allow(clippy::too_many_arguments)] // Shares the same captured scan inputs and depth budget.
    fn scan_bodies(
        &mut self,
        docs: &[&DocumentInput],
        node: Node<'_>,
        scope: usize,
        declarations: &BTreeMap<(usize, usize, usize), Vec<&Declaration>>,
        edges: &mut Vec<ModuleEdge>,
        depth: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        self.step(check)?;
        ensure!(depth <= DEPTH, "bounded_work: reference AST");
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "block" {
                let block = self.add_scope(
                    self.scopes[scope].document,
                    child,
                    Some(scope),
                    self.scopes[scope].directory.clone(),
                    false,
                    check,
                )?;
                self.bind_parameters(docs, child, block, check)?;
                self.scan(docs, child, block, declarations, edges, depth + 1, check)?;
            } else if !opaque(child) {
                self.scan_bodies(docs, child, scope, declarations, edges, depth + 1, check)?;
            }
        }
        Ok(())
    }

    fn scope_at(
        &mut self,
        document: usize,
        start: usize,
        end: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<usize>> {
        self.step(check)?;
        let Some(index) = self.scope_indexes.get(&document) else {
            return Ok(None);
        };
        index.enclosing(start, end, &mut self.work, check)
    }
    fn root(&self, scope: usize) -> Option<usize> {
        let module = self.scopes[scope].module;
        if self.ambiguous.contains(&module) {
            return None;
        }
        let roots = self.roots.get(&module)?;
        (roots.len() == 1).then(|| *roots.first().expect("one root"))
    }
    fn parent_module(&self, module: usize) -> Option<usize> {
        self.scopes[module].parent.map(|p| self.scopes[p].module)
    }
    #[allow(clippy::too_many_arguments)] // Captured context and route remain explicit.
    fn binding(
        &mut self,
        scope: usize,
        name: &str,
        exported: bool,
        depth: usize,
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        self.step(check)?;
        let mut result = Names::default();
        if depth > FOLLOW {
            result.unknown_set = true;
            return Ok(result);
        }
        if !self.enter_route(route, (scope, name.to_owned()), check)? {
            result.unknown_set = true;
            return Ok(result);
        }
        let mut direct = Vec::new();
        for namespace in [TYPE, VALUE] {
            self.step(check)?;
            let key = (namespace, name.to_owned());
            if let Some(bindings) = self.scopes[scope].explicit.get(&key) {
                let count = bindings.len();
                self.charge(count, check)?;
                direct.push((
                    key,
                    self.scopes[scope].explicit[&(namespace, name.to_owned())].clone(),
                ));
            }
        }
        let mut explicit_namespaces = BTreeSet::new();
        for (key, bindings) in direct {
            explicit_namespaces.insert(key.0);
            let bucket = result.names.entry(key).or_default();
            for binding in bindings {
                self.step(check)?;
                if exported && !binding.public {
                    continue;
                }
                if binding.uncertain {
                    bucket.unknown = true;
                } else if let Some(mut candidate) = binding.candidate {
                    candidate.follow = candidate.follow.max(depth);
                    bucket
                        .candidates
                        .insert(candidate.declaration.clone(), candidate);
                } else {
                    bucket.unknown = true;
                }
            }
        }
        let imports = self.scope_imports(scope, check)?;
        for import in imports
            .iter()
            .filter(|i| !i.star && i.binding_name() == Some(name))
        {
            let mut resolved = if import.uncertain {
                Names {
                    unknown_set: true,
                    ..Names::default()
                }
            } else {
                self.path(scope, &import.path, depth + 1, route, context, check)?
            };
            if resolved.unknown_set && (!exported || import.public) {
                // An unproved named import has a known spelling but unproved namespace.
                for namespace in [TYPE, VALUE] {
                    result
                        .names
                        .entry((namespace, name.to_owned()))
                        .or_default()
                        .unknown = true;
                    explicit_namespaces.insert(namespace);
                }
            }
            if resolved.unknown_set {
                explicit_namespaces.extend([TYPE, VALUE]);
            }
            for ((namespace, _), bucket) in std::mem::take(&mut resolved.names) {
                explicit_namespaces.insert(namespace);
                if exported && !import.public {
                    continue;
                }
                self.merge_bucket(
                    result
                        .names
                        .entry((namespace, name.to_owned()))
                        .or_default(),
                    bucket,
                    check,
                )?;
            }
        }
        explicit_namespaces.extend(
            result
                .names
                .iter()
                .filter(|(_, b)| b.unknown || !b.candidates.is_empty())
                .map(|((ns, _), _)| *ns),
        );
        let mut stars = Names::default();
        for import in imports.iter().filter(|i| i.star && (!exported || i.public)) {
            if self.waived_star(scope, import, context, check)? {
                continue;
            }
            let expanded =
                self.expand(scope, import, depth + 1, route, context, Some(name), check)?;
            stars.unknown_set |= expanded.unknown_set;
            for (key, bucket) in expanded
                .names
                .into_iter()
                .filter(|((_, spelling), _)| spelling == name)
            {
                self.merge_bucket(stars.names.entry(key).or_default(), bucket, check)?;
            }
        }
        for (key, mut bucket) in stars.names {
            if explicit_namespaces.contains(&key.0) {
                continue;
            }
            bucket.unknown |= stars.unknown_set;
            result.names.insert(key, bucket);
        }
        if stars.unknown_set {
            for namespace in [TYPE, VALUE] {
                if !explicit_namespaces.contains(&namespace) {
                    result
                        .names
                        .entry((namespace, name.to_owned()))
                        .or_default()
                        .unknown = true;
                }
            }
        }
        if self.scopes[scope].opaque {
            for namespace in [TYPE, VALUE] {
                if !explicit_namespaces.contains(&namespace) {
                    result
                        .names
                        .entry((namespace, name.to_owned()))
                        .or_default()
                        .unknown = true;
                }
            }
        }
        let document = self.scopes[scope].document;
        for bucket in result.names.values_mut() {
            for candidate in bucket.candidates.values_mut() {
                candidate.proof.insert(document);
            }
        }
        route.remove(&(scope, name.to_owned()));
        Ok(result)
    }
    fn merge_bucket(
        &mut self,
        into: &mut Bucket,
        from: Bucket,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        into.unknown |= from.unknown;
        for (identity, candidate) in from.candidates {
            self.step(check)?;
            self.charge(candidate.proof.len(), check)?;
            if let Some(existing) = into.candidates.get_mut(&identity) {
                existing.follow = existing.follow.min(candidate.follow);
                existing.proof.extend(candidate.proof);
            } else {
                into.candidates.insert(identity, candidate);
            }
        }
        Ok(())
    }
    fn private_access(
        &mut self,
        site: usize,
        target: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        let mut current = Some(self.scopes[site].module);
        let target = self.scopes[target].module;
        let mut seen = BTreeSet::new();
        while let Some(module) = current {
            self.step(check)?;
            if module == target {
                return Ok(true);
            }
            if !seen.insert(module) {
                break;
            }
            current = self.parent_module(module);
        }
        Ok(false)
    }
    fn path(
        &mut self,
        scope: usize,
        path: &[String],
        depth: usize,
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        self.step(check)?;
        if depth > FOLLOW || path.is_empty() || self.root(scope).is_none() {
            return Ok(Names {
                unknown_set: true,
                ..Names::default()
            });
        }
        let mut depth = depth;
        let mut index = 0;
        let mut current = self.scopes[scope].module;
        let mut lexical = false;
        match path[0].as_str() {
            "crate" => {
                current = self.root(scope).expect("proved root");
                index = 1;
            }
            "self" => {
                index = 1;
            }
            "super" => {
                while path.get(index).is_some_and(|s| s == "super") {
                    if let Some(parent) = self.parent_module(current) {
                        current = parent;
                        index += 1;
                    } else {
                        return Ok(Names {
                            unknown_set: true,
                            ..Names::default()
                        });
                    }
                }
            }
            _ => {
                current = scope;
                lexical = true;
            }
        }
        if index == path.len() {
            // A module prefix alone is useful as an expansion target, not a synthetic declaration.
            let mut names = Names::default();
            let document = self.scopes[current].document;
            let candidate = Candidate {
                declaration: self.file_declarations[&document].clone(),
                module: Some(current),
                follow: depth,
                proof: BTreeSet::from([document]),
            };
            names
                .names
                .entry((TYPE, path[index - 1].clone()))
                .or_default()
                .candidates
                .insert(candidate.declaration.clone(), candidate);
            return Ok(names);
        }
        let mut proof = BTreeSet::new();
        loop {
            self.step(check)?;
            proof.insert(self.scopes[current].document);
            let exported = current == self.scopes[current].module
                && !self.private_access(scope, current, check)?;
            let mut found = self.binding(
                current,
                &path[index],
                exported,
                depth,
                route,
                context,
                check,
            )?;
            if lexical
                && current != self.scopes[current].module
                && found
                    .names
                    .values()
                    .all(|b| b.candidates.is_empty() && !b.unknown)
                && !found.unknown_set
                && let Some(parent) = self.scopes[current].parent
            {
                current = parent;
                continue;
            }
            lexical = false;
            for bucket in found.names.values_mut() {
                for candidate in bucket.candidates.values_mut() {
                    candidate.proof.extend(proof.iter().copied());
                }
            }
            index += 1;
            if index == path.len() {
                if found
                    .names
                    .values()
                    .all(|b| b.candidates.is_empty() && !b.unknown)
                {
                    found.unknown_set = true;
                }
                return Ok(found);
            }
            let modules = found.names.get(&(TYPE, path[index - 1].clone()));
            let next = modules
                .filter(|b| !b.unknown && b.candidates.len() == 1)
                .and_then(|b| b.candidates.values().next());
            if found.unknown_set || next.and_then(|c| c.module).is_none() {
                return Ok(Names {
                    unknown_set: true,
                    ..Names::default()
                });
            }
            let next = next.expect("proved module candidate");
            proof.extend(next.proof.iter().copied());
            depth = depth.max(next.follow);
            current = next.module.expect("proved module");
            if self.root(current) != self.root(scope) {
                return Ok(Names {
                    unknown_set: true,
                    ..Names::default()
                });
            }
        }
    }
    #[allow(clippy::too_many_arguments)] // Select a spelling without copying the full export set.
    fn expand(
        &mut self,
        scope: usize,
        import: &Import,
        depth: usize,
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        spelling: Option<&str>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        self.step(check)?;
        if import.uncertain || depth > FOLLOW {
            return Ok(Names {
                unknown_set: true,
                ..Names::default()
            });
        }
        let target = self.path(scope, &import.path, depth, route, context, check)?;
        let mut modules = Vec::new();
        let mut unknown = target.unknown_set;
        for ((namespace, _), bucket) in target.names {
            if namespace != TYPE {
                continue;
            }
            unknown |= bucket.unknown || bucket.candidates.len() != 1;
            for candidate in bucket.candidates.values() {
                if let Some(module) = candidate.module {
                    modules.push((module, candidate.proof.clone(), depth.max(candidate.follow)));
                } else {
                    unknown = true;
                }
            }
        }
        if modules.len() != 1 {
            return Ok(Names {
                unknown_set: true,
                ..Names::default()
            });
        }
        let (module, mut proof, depth) = modules.pop().expect("one module");
        proof.insert(self.scopes[module].document);
        let exported = self.exports(module, depth, route, context, check)?;
        let mut result = self.copy_names(&exported, spelling, check)?;
        result.unknown_set |= unknown;
        for bucket in result.names.values_mut() {
            for candidate in bucket.candidates.values_mut() {
                self.charge(proof.len(), check)?;
                candidate.proof.extend(proof.iter().copied());
            }
        }
        Ok(result)
    }
    fn enter_route(
        &mut self,
        route: &mut Route,
        key: RouteKey,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        self.step(check)?;
        let inserted = route.insert(key.clone());
        self.charge(self.export_traces.len(), check)?;
        for trace in &mut self.export_traces {
            trace.consulted.insert(key.clone());
            trace.cycle |= !inserted;
        }
        Ok(inserted)
    }
    fn waived_star(
        &mut self,
        scope: usize,
        import: &Import,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        if !context.waive_external || import.uncertain {
            return Ok(false);
        }
        Ok(self
            .external_target(
                scope,
                &import.path,
                context.packages,
                context.docs,
                Some((import.start, import.end)),
                check,
            )?
            .is_some())
    }
    fn copy_names(
        &mut self,
        names: &Names,
        spelling: Option<&str>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        let mut copied = Names {
            names: BTreeMap::new(),
            unknown_set: names.unknown_set,
        };
        if let Some(spelling) = spelling {
            for namespace in [TYPE, VALUE] {
                self.step(check)?;
                let key = (namespace, spelling.to_owned());
                if let Some(bucket) = names.names.get(&key) {
                    self.copy_bucket(&mut copied, key, bucket, check)?;
                }
            }
        } else {
            for (key, bucket) in &names.names {
                self.step(check)?;
                self.copy_bucket(&mut copied, key.clone(), bucket, check)?;
            }
        }
        Ok(copied)
    }
    fn copy_bucket(
        &mut self,
        into: &mut Names,
        key: Name,
        bucket: &Bucket,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        self.step(check)?;
        for candidate in bucket.candidates.values() {
            self.step(check)?;
            self.charge(candidate.proof.len(), check)?;
        }
        into.names.insert(key, bucket.clone());
        Ok(())
    }
    fn exports(
        &mut self,
        module: usize,
        depth: usize,
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<std::sync::Arc<Names>> {
        self.step(check)?;
        if depth > FOLLOW {
            return Ok(std::sync::Arc::new(Names {
                unknown_set: true,
                ..Names::default()
            }));
        }
        // Depth and waiver mode affect unknown sets. Cached computations are valid only
        // when none of their consulted route keys is already visiting in this caller.
        let key = (module, depth, context.waive_external);
        if let Some(memo) = self.export_memo.get(&key).cloned() {
            self.charge(memo.consulted.len(), check)?;
            if memo.consulted.is_disjoint(route) {
                for consulted in &memo.consulted {
                    self.charge(self.export_traces.len(), check)?;
                    for trace in &mut self.export_traces {
                        trace.consulted.insert(consulted.clone());
                    }
                }
                return Ok(memo.names.clone());
            }
        }
        self.export_traces.push(ExportTrace::default());
        let route_key = (module, "*".to_owned());
        let already_visiting = route.contains(&route_key);
        let resolved = (|| {
            if !self.enter_route(route, route_key.clone(), check)? {
                return Ok(Names {
                    unknown_set: true,
                    ..Names::default()
                });
            }
            self.compute_exports(module, depth, route, context, check)
        })();
        // Only remove our own insertion; a rejected insertion belongs to the caller.
        let trace = self.export_traces.pop().expect("export trace");
        if !already_visiting {
            route.remove(&route_key);
        }
        let names = std::sync::Arc::new(resolved?);
        if !trace.cycle {
            self.charge(trace.consulted.len(), check)?;
            self.export_memo.insert(
                key,
                std::sync::Arc::new(ExportMemo {
                    names: names.clone(),
                    consulted: trace.consulted,
                }),
            );
        }
        Ok(names)
    }
    fn compute_exports(
        &mut self,
        module: usize,
        depth: usize,
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        self.step(check)?;
        let mut spellings = BTreeSet::new();
        self.charge(self.scopes[module].explicit.len(), check)?;
        let keys: Vec<_> = self.scopes[module].explicit.keys().cloned().collect();
        for key in keys {
            self.step(check)?;
            let count = self.scopes[module].explicit[&key].len();
            self.charge(count, check)?;
            if self.scopes[module].explicit[&key].iter().any(|b| b.public) {
                spellings.insert(key.1);
            }
        }
        let imports = self.scope_imports(module, check)?;
        let mut result = Names {
            unknown_set: self.scopes[module].opaque,
            ..Names::default()
        };
        for import in imports.iter().filter(|i| i.public) {
            self.step(check)?;
            if import.star {
                if self.waived_star(module, import, context, check)? {
                    continue;
                }
                let expanded =
                    self.expand(module, import, depth + 1, route, context, None, check)?;
                result.unknown_set |= expanded.unknown_set;
                self.charge(expanded.names.len(), check)?;
                spellings.extend(expanded.names.into_keys().map(|(_, name)| name));
            } else if let Some(name) = import.binding_name() {
                spellings.insert(name.to_owned());
            }
        }
        for name in spellings {
            self.step(check)?;
            let resolved = self.binding(module, &name, true, depth, route, context, check)?;
            result.unknown_set |= resolved.unknown_set;
            for (key, bucket) in resolved.names {
                result.names.insert(key, bucket);
            }
        }
        Ok(result)
    }
    fn external_target(
        &mut self,
        scope: usize,
        path: &[String],
        packages: &external::RustPackages,
        docs: &[&DocumentInput],
        excluding: Option<(usize, usize)>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<external::ExternalPackage>> {
        self.step(check)?;
        let Some(first) = path.first() else {
            return Ok(None);
        };
        if matches!(first.as_str(), "crate" | "self" | "super") || self.root(scope).is_none() {
            return Ok(None);
        }
        let mut current = Some(scope);
        while let Some(current_scope) = current {
            self.step(check)?;
            if self.scopes[current_scope].opaque {
                return Ok(None);
            }
            if self.scopes[current_scope]
                .explicit
                .contains_key(&(TYPE, first.clone()))
            {
                return Ok(None);
            }
            let imports = self.scope_imports(current_scope, check)?;
            for import in imports {
                self.step(check)?;
                if excluding == Some((import.start, import.end)) && current_scope == scope {
                    continue;
                }
                if !import.star && import.binding_name() == Some(first) {
                    return Ok(None);
                }
                if import.star {
                    let expanded = self.expand(
                        current_scope,
                        &import,
                        1,
                        &mut BTreeSet::new(),
                        ResolveContext {
                            packages,
                            docs,
                            waive_external: false,
                        },
                        Some(first),
                        check,
                    )?;
                    if expanded
                        .names
                        .get(&(TYPE, first.clone()))
                        .is_some_and(|b| b.unknown || !b.candidates.is_empty())
                    {
                        return Ok(None);
                    }
                    if expanded.unknown_set {
                        let document = self.scopes[current_scope].document;
                        let source_set = &docs[document].key.source_set_id;
                        let root = self.root(current_scope).expect("proved root");
                        let root_path = &docs[self.scopes[root].document].key.path;
                        let declared_external = !import.uncertain
                            && import.path.first().is_some_and(|name| {
                                !self.scopes[current_scope]
                                    .explicit
                                    .contains_key(&(TYPE, name.clone()))
                                    && packages
                                        .resolve(
                                            source_set,
                                            name,
                                            packages.root_allowed(source_set, root_path),
                                        )
                                        .is_some()
                            });
                        if !declared_external {
                            return Ok(None);
                        }
                    }
                }
            }
            current = if current_scope == self.scopes[current_scope].module {
                None
            } else {
                self.scopes[current_scope].parent
            };
        }
        let document = self.scopes[scope].document;
        let root = self.root(scope).expect("proved root");
        let root_path = &docs[self.scopes[root].document].key.path;
        let source_set = &docs[document].key.source_set_id;
        Ok(packages.resolve(
            source_set,
            first,
            packages.root_allowed(source_set, root_path),
        ))
    }
    pub(super) fn resolve(
        &mut self,
        document: usize,
        site: &RawSite,
        packages: &external::RustPackages,
        external_packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
        docs: &[&DocumentInput],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        self.step(check)?;
        let Some(scope) = self.scope_at(document, site.start, site.end, check)? else {
            return Ok(SiteResolution::unknown("rust_scope_unproved"));
        };
        if self.root(scope).is_none() {
            return Ok(SiteResolution::unknown("rust_module_ownership_unproved"));
        }
        let imports = self.scope_imports(scope, check)?;
        let mut import = None;
        for candidate in imports {
            self.step(check)?;
            if candidate.start == site.start && candidate.end == site.end {
                import = Some(candidate);
                break;
            }
        }
        let path = import
            .as_ref()
            .map(|i| i.path.clone())
            .or_else(|| parts(&site.spelling));
        if let Some(path) = &path
            && !import.as_ref().is_some_and(|i| i.uncertain)
            && let Some(package) = self.external_target(
                scope,
                path,
                packages,
                docs,
                import.as_ref().map(|i| (i.start, i.end)),
                check,
            )?
        {
            let identity = package.identity.clone();
            self.charge(package.evidence.len(), check)?;
            if let Some(existing) = external_packages.get_mut(&identity) {
                existing.evidence.extend(package.evidence);
                existing.evidence.sort();
                existing.evidence.dedup();
            } else {
                external_packages.insert(identity.clone(), package);
            }
            return Ok(SiteResolution {
                targets: BTreeSet::from([Target::External(identity)]),
                ..SiteResolution::default()
            });
        }
        let context = ResolveContext {
            packages,
            docs,
            waive_external: true,
        };
        let mut route = BTreeSet::new();
        let mut shadowed = false;
        let mut names = if let Some(import) = &import {
            if import.uncertain {
                return Ok(SiteResolution::unknown("rust_conditional_export"));
            }
            if import.star {
                let mut own = self.expand(scope, import, 1, &mut route, context, None, check)?;
                let others = self.scope_imports(scope, check)?;
                let mut competing = Names::default();
                for other in others
                    .iter()
                    .filter(|i| i.star && (i.start != import.start || i.end != import.end))
                {
                    if self
                        .external_target(
                            scope,
                            &other.path,
                            packages,
                            docs,
                            Some((other.start, other.end)),
                            check,
                        )?
                        .is_some()
                    {
                        continue;
                    }
                    let expanded =
                        self.expand(scope, other, 1, &mut route, context, None, check)?;
                    competing.unknown_set |= expanded.unknown_set;
                    for (key, bucket) in expanded.names {
                        self.merge_bucket(competing.names.entry(key).or_default(), bucket, check)?;
                    }
                }
                self.charge(own.names.len(), check)?;
                let keys: Vec<_> = own.names.keys().cloned().collect();
                for key in keys {
                    self.step(check)?;
                    let direct = self.scopes[scope].explicit.contains_key(&key);
                    let mut explicit_imports = Vec::new();
                    for other in &others {
                        self.step(check)?;
                        if !other.star && other.binding_name() == Some(key.1.as_str()) {
                            self.step(check)?;
                            explicit_imports.push(other.clone());
                        }
                    }
                    let mut explicit = direct;
                    for named in explicit_imports {
                        let resolved = if named.uncertain {
                            Names {
                                unknown_set: true,
                                ..Names::default()
                            }
                        } else {
                            self.path(scope, &named.path, 1, &mut route, context, check)?
                        };
                        explicit |= resolved.unknown_set
                            || resolved
                                .names
                                .keys()
                                .any(|(namespace, _)| *namespace == key.0);
                    }
                    if explicit {
                        own.names.remove(&key);
                        shadowed = true;
                        continue;
                    }
                    let bucket = own.names.get_mut(&key).expect("existing name");
                    bucket.unknown |= competing.unknown_set || self.scopes[scope].opaque;
                    if let Some(other) = competing.names.get(&key) {
                        self.merge_bucket(bucket, other.clone(), check)?;
                    }
                }
                own
            } else {
                self.path(scope, &import.path, 1, &mut route, context, check)?
            }
        } else {
            match parts(&site.spelling) {
                Some(path) => self.path(scope, &path, 1, &mut route, context, check)?,
                None => return Ok(SiteResolution::unknown("rust_path_unsupported")),
            }
        };
        let mut result = SiteResolution::default();
        if names.unknown_set {
            result.residual = Some("rust_export_name_set_unknown");
        }
        for ((namespace, _), bucket) in std::mem::take(&mut names.names) {
            if site.namespace.is_some_and(|wanted| wanted != namespace) {
                continue;
            }
            self.step(check)?;
            if bucket.unknown || bucket.candidates.len() > 1 {
                result.residual = Some("rust_name_ambiguous");
                continue;
            }
            for candidate in bucket.candidates.into_values() {
                result
                    .targets
                    .insert(Target::Native(candidate.declaration.document));
                result.proof.extend(candidate.proof);
            }
        }
        if result.targets.is_empty() && result.residual.is_none() {
            result.residual = Some(if import.as_ref().is_some_and(|i| i.star) {
                if shadowed {
                    "rust_wildcard_fully_shadowed"
                } else {
                    "rust_wildcard_empty"
                }
            } else {
                "rust_name_unproved"
            });
        }
        Ok(result)
    }
}
fn visibility<'a>(doc: &'a DocumentInput, node: Node<'_>) -> Option<&'a str> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find(|n| n.kind() == "visibility_modifier")
        .map(|n| text(doc, n))
}
fn parts(value: &str) -> Option<Vec<String>> {
    let parts: Vec<_> = value.split("::").map(str::trim).collect();
    (!parts.is_empty()
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c == '_' || c.is_alphanumeric())))
    .then(|| parts.into_iter().map(str::to_owned).collect())
}
impl Import {
    fn binding_name(&self) -> Option<&str> {
        if self.star || self.alias.as_deref() == Some("_") {
            None
        } else {
            self.alias
                .as_deref()
                .or_else(|| self.path.last().map(String::as_str))
        }
    }
}
fn imports(
    doc: &DocumentInput,
    root: Node<'_>,
    public: bool,
    uncertain: bool,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Import>> {
    let mut pending = vec![(root, Vec::<String>::new())];
    let mut result = Vec::new();
    while let Some((node, prefix)) = pending.pop() {
        owner_step(work, check)?;
        let combine = |value: &str| -> Vec<String> {
            let mut combined = prefix.clone();
            if let Some(tail) = parts(value) {
                if tail == ["self"] && !combined.is_empty() {
                    return combined;
                }
                combined.extend(tail);
            }
            combined
        };
        match node.kind() {
            "scoped_use_list" => {
                let path = node
                    .child_by_field_name("path")
                    .map(|n| text(doc, n))
                    .unwrap_or("");
                if let Some(list) = node.child_by_field_name("list") {
                    pending.push((list, combine(path)));
                }
            }
            "use_list" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    owner_step(work, check)?;
                    pending.push((child, prefix.clone()));
                }
            }
            _ => {
                let star = node.kind() == "use_wildcard";
                let (path, alias) = if node.kind() == "use_as_clause" {
                    (
                        node.child_by_field_name("path")
                            .map(|n| text(doc, n))
                            .unwrap_or(""),
                        node.child_by_field_name("alias")
                            .map(|n| text(doc, n).to_owned()),
                    )
                } else if star {
                    (
                        text(doc, node)
                            .trim_end_matches('*')
                            .trim_end()
                            .trim_end_matches("::"),
                        None,
                    )
                } else {
                    (text(doc, node), None)
                };
                result.push(Import {
                    start: node.start_byte(),
                    end: node.end_byte(),
                    path: combine(path),
                    alias,
                    star,
                    public,
                    uncertain,
                });
            }
        }
    }
    Ok(result)
}

pub(super) fn extract(
    doc: &DocumentInput,
    tree: &Tree,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<RawSite>> {
    let mut result = Vec::new();
    walk(tree.root_node(), check, &mut |node, check| {
        if opaque(node) {
            return Ok(true);
        }
        if node.kind() == "use_declaration" {
            if let Some(argument) = node.child_by_field_name("argument") {
                for import in imports(doc, argument, false, false, work, check)? {
                    ensure!(result.len() < SITES, "bounded_work: reference site count");
                    result.push(RawSite {
                        start: import.start,
                        end: import.end,
                        kind: "import",
                        spelling: doc.file.text[import.start..import.end].to_owned(),
                        destination: Ok(import.path),
                        namespace: None,
                    });
                }
            }
            return Ok(true);
        }
        if matches!(node.kind(), "scoped_identifier" | "scoped_type_identifier") {
            ensure!(result.len() < SITES, "bounded_work: reference site count");
            let mut site = raw(doc, node, "reference", Ok(Vec::new()));
            site.namespace = Some(if node.kind() == "scoped_type_identifier" {
                TYPE
            } else {
                VALUE
            });
            result.push(site);
            return Ok(true);
        }
        Ok(false)
    })?;
    result.sort_by(|a, b| {
        (a.start, a.end, a.kind, &a.spelling).cmp(&(b.start, b.end, b.kind, &b.spelling))
    });
    Ok(result)
}

#[cfg(test)]
mod cache_tests {
    use super::*;

    fn scope(document: usize, module: usize) -> Scope {
        Scope {
            document,
            start: 0,
            end: 100,
            parent: None,
            module,
            directory: "src".into(),
            explicit: BTreeMap::new(),
            imports: Vec::new(),
            opaque: false,
        }
    }

    fn candidate(document: usize, module: Option<usize>, proof: &[usize]) -> Candidate {
        Candidate {
            declaration: DeclarationId {
                document,
                start: 10,
                end: 20,
                sid: format!("fixture-{document}"),
            },
            module,
            follow: 0,
            proof: proof.iter().copied().collect(),
        }
    }

    fn bind(scope: &mut Scope, namespace: u8, name: &str, candidate: Candidate) {
        scope.explicit.insert(
            (namespace, name.into()),
            vec![Binding {
                candidate: Some(candidate),
                public: true,
                uncertain: false,
            }],
        );
    }

    fn star(path: &[&str]) -> Import {
        Import {
            start: 30,
            end: 40,
            path: path.iter().map(|s| (*s).to_owned()).collect(),
            alias: None,
            star: true,
            public: true,
            uncertain: false,
        }
    }

    fn catalog(scopes: Vec<Scope>) -> Catalog {
        Catalog {
            roots: (0..scopes.len())
                .map(|id| (id, BTreeSet::from([0])))
                .collect(),
            scopes,
            scope_indexes: BTreeMap::new(),
            export_memo: BTreeMap::new(),
            export_traces: Vec::new(),
            files: BTreeMap::new(),
            file_declarations: BTreeMap::new(),
            ambiguous: BTreeSet::new(),
            work: Work::new(work::reference_limit(0, 0)),
        }
    }

    fn exported_function(names: &Names, depth: usize, proof: &[usize]) {
        assert!(!names.unknown_set);
        assert_eq!(names.names.len(), 1);
        let bucket = &names.names[&(VALUE, "f".into())];
        assert!(!bucket.unknown);
        assert_eq!(bucket.candidates.len(), 1);
        let candidate = bucket.candidates.values().next().unwrap();
        assert_eq!(candidate.follow, depth);
        assert_eq!(candidate.proof, proof.iter().copied().collect());
    }

    #[test]
    fn export_cache_never_reuses_or_publishes_blocked_name_or_star_routes() {
        let packages = external::RustPackages::default();
        let context = ResolveContext {
            packages: &packages,
            docs: &[],
            waive_external: false,
        };
        for blocked_name in ["f", "*"] {
            for warm_first in [false, true] {
                let mut module = scope(0, 0);
                bind(&mut module, VALUE, "f", candidate(0, None, &[0]));
                let mut catalog = catalog(vec![module]);
                let mut check = || Ok(());
                if warm_first {
                    let names = catalog
                        .exports(0, 1, &mut Route::new(), context, &mut check)
                        .unwrap();
                    exported_function(&names, 1, &[0]);
                }
                let expected_route = Route::from([(0, blocked_name.into())]);
                let mut route = expected_route.clone();
                let blocked = catalog
                    .exports(0, 1, &mut route, context, &mut check)
                    .unwrap();
                assert!(blocked.unknown_set);
                assert!(blocked.names.is_empty());
                assert_eq!(route, expected_route);
                let mut route = Route::new();
                let names = catalog
                    .exports(0, 1, &mut route, context, &mut check)
                    .unwrap();
                exported_function(&names, 1, &[0]);
                assert!(route.is_empty());
            }
        }
    }

    #[test]
    fn nested_cache_hits_propagate_name_and_star_footprints_to_outer_exports() {
        let packages = external::RustPackages::default();
        let context = ResolveContext {
            packages: &packages,
            docs: &[],
            waive_external: false,
        };
        for blocked_name in ["f", "*"] {
            for warm_first in [false, true] {
                let mut outer = scope(0, 0);
                outer.parent = Some(1);
                outer.imports.push(star(&["super"]));
                let mut inner = scope(1, 1);
                bind(&mut inner, VALUE, "f", candidate(1, None, &[1]));
                let mut catalog = catalog(vec![outer, inner]);
                catalog
                    .file_declarations
                    .insert(1, candidate(1, Some(1), &[1]).declaration);
                let mut check = || Ok(());
                // This completes before the outer computation, whose expansion must hit it.
                let inner = catalog
                    .exports(1, 2, &mut Route::new(), context, &mut check)
                    .unwrap();
                exported_function(&inner, 2, &[1]);
                if warm_first {
                    let names = catalog
                        .exports(0, 1, &mut Route::new(), context, &mut check)
                        .unwrap();
                    exported_function(&names, 2, &[0, 1]);
                }
                let expected_route = Route::from([(1, blocked_name.into())]);
                let mut route = expected_route.clone();
                let blocked = catalog
                    .exports(0, 1, &mut route, context, &mut check)
                    .unwrap();
                assert!(blocked.unknown_set);
                assert!(blocked.names.is_empty());
                assert_eq!(route, expected_route);
                let names = catalog
                    .exports(0, 1, &mut Route::new(), context, &mut check)
                    .unwrap();
                exported_function(&names, 2, &[0, 1]);
            }
        }
    }

    #[test]
    fn export_cache_separates_follow_depth_and_external_waiver_mode() {
        let packages = external::RustPackages::default();
        let document = DocumentInput {
            semantic: Default::default(),
            key: DocumentKey {
                source_set_id: "fixture".into(),
                language: "rust".into(),
                path: "src/lib.rs".into(),
            },
            file: crate::model::SourceFile {
                path: "src/lib.rs".into(),
                hash: String::new(),
                language: "rust".into(),
                text: String::new(),
            },
            declarations: Vec::new(),
            calls: Vec::new(),
        };
        let docs = [&document];
        let mut module = scope(0, 0);
        bind(&mut module, VALUE, "f", candidate(0, None, &[0]));
        module.imports.push(star(&["std", "collections"]));
        let mut mode_catalog = catalog(vec![module]);
        let mut check = || Ok(());
        for (depth, waive_external) in [
            (1, true),
            (FOLLOW, false),
            (FOLLOW, true),
            (1, false),
            (1, true),
        ] {
            let context = ResolveContext {
                packages: &packages,
                docs: &docs,
                waive_external,
            };
            let names = mode_catalog
                .exports(0, depth, &mut Route::new(), context, &mut check)
                .unwrap();
            assert_eq!(names.unknown_set, !waive_external);
            let bucket = &names.names[&(VALUE, "f".into())];
            assert!(!bucket.unknown);
            assert_eq!(bucket.candidates.len(), 1);
            assert_eq!(bucket.candidates.values().next().unwrap().follow, depth);
        }
        let mut forwarding = scope(0, 0);
        forwarding.parent = Some(1);
        forwarding.imports.push(star(&["super"]));
        let mut origin = scope(1, 1);
        bind(&mut origin, VALUE, "f", candidate(1, None, &[1]));
        let mut forwarding_catalog = catalog(vec![forwarding, origin]);
        forwarding_catalog
            .file_declarations
            .insert(1, candidate(1, Some(1), &[1]).declaration);
        let context = ResolveContext {
            packages: &packages,
            docs: &[],
            waive_external: false,
        };
        for depth in [1, FOLLOW, 1] {
            let names = forwarding_catalog
                .exports(0, depth, &mut Route::new(), context, &mut check)
                .unwrap();
            if depth == 1 {
                exported_function(&names, 2, &[0, 1]);
            } else {
                assert!(names.unknown_set);
                assert!(names.names.is_empty());
            }
        }
    }

    #[test]
    fn shared_exports_keep_importing_path_proofs_out_of_later_callers() {
        let packages = external::RustPackages::default();
        let context = ResolveContext {
            packages: &packages,
            docs: &[],
            waive_external: false,
        };
        let mut root = scope(0, 0);
        bind(&mut root, TYPE, "left", candidate(2, Some(1), &[0, 2]));
        bind(&mut root, TYPE, "right", candidate(3, Some(1), &[0, 3]));
        let mut target = scope(1, 1);
        target.parent = Some(0);
        bind(&mut target, VALUE, "f", candidate(1, None, &[1]));
        let mut catalog = catalog(vec![root, target]);
        let mut check = || Ok(());
        for (name, proof) in [
            ("left", vec![0, 1, 2]),
            ("right", vec![0, 1, 3]),
            ("left", vec![0, 1, 2]),
        ] {
            let names = catalog
                .expand(
                    0,
                    &star(&["crate", name]),
                    1,
                    &mut Route::new(),
                    context,
                    None,
                    &mut check,
                )
                .unwrap();
            exported_function(&names, 1, &proof);
        }
        let names = catalog
            .exports(1, 1, &mut Route::new(), context, &mut check)
            .unwrap();
        exported_function(&names, 1, &[1]);
    }

    #[test]
    fn catalog_steps_use_the_injected_request_limit() {
        let mut catalog = catalog(Vec::new());
        catalog.work = Work::new(1);
        catalog.step(&mut || Ok(())).unwrap();
        let error = catalog.step(&mut || Ok(())).unwrap_err();
        assert!(error.to_string().starts_with("bounded_work:"));
        assert_eq!(catalog.work.used, 2);
    }

    #[test]
    fn scope_index_preserves_smallest_range_and_reverse_id_tie_break() {
        let mut index = ScopeIndex {
            ranges: vec![
                (0, 0, 100),
                (1, 10, 90),
                (2, 20, 40),
                (3, 20, 40),
                (4, 60, 80),
            ],
            max_ends: vec![0; 5],
        };
        let mut work = Work::new(work::reference_limit(0, 0));
        let mut check = || Ok(());
        index
            .build(0, index.ranges.len(), &mut work, &mut check)
            .unwrap();
        assert_eq!(
            index.enclosing(25, 30, &mut work, &mut check).unwrap(),
            Some(3)
        );
        assert_eq!(
            index.enclosing(40, 60, &mut work, &mut check).unwrap(),
            Some(1)
        );
        assert_eq!(
            index.enclosing(65, 70, &mut work, &mut check).unwrap(),
            Some(4)
        );
        assert_eq!(
            index.enclosing(101, 102, &mut work, &mut check).unwrap(),
            None
        );
    }
}
