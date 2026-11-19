//! Bounded, captured-syntax Rust name and module proofs, not compiler bindings.
//! Foreign-block items and derive-generated sibling names are not modeled.
use super::shared::DeclarationId;
use super::*;

type RouteKey = (usize, String);
type Route = BTreeSet<RouteKey>;

#[derive(Clone, Copy)]
struct ResolveContext<'a, 'b> {
    packages: &'a external::RustPackages,
    docs: &'a [&'b DocumentInput],
    waive_external: bool,
    /// The lookup reached this crate from another workspace member (#772), so `pub(crate)`
    /// names are not visible.
    foreign: bool,
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
/// The declarations of the Rust files, by document and range.
type DeclarationsByRange<'a> = BTreeMap<(usize, usize, usize), Vec<&'a Declaration>>;
const TYPE: u8 = 0;
const VALUE: u8 = 1;

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
    /// `pub(crate)`: visible in its own crate only.
    crate_only: bool,
    uncertain: bool,
}
/// A scope's imports in source order. Named imports are also indexed by binding name and star
/// imports listed, so a name lookup reads only the imports that can bind it (#894).
#[derive(Default)]
struct Imports {
    all: Vec<Import>,
    named: BTreeMap<String, Vec<usize>>,
    stars: Vec<usize>,
}
impl Imports {
    fn push(&mut self, import: Import) {
        let index = self.all.len();
        if import.star {
            self.stars.push(index);
        } else if let Some(name) = import.binding_name() {
            self.named.entry(name.to_owned()).or_default().push(index);
        }
        self.all.push(import);
    }
    /// The named imports binding `name`, in source order.
    fn named(&self, name: &str) -> impl Iterator<Item = &Import> {
        self.named
            .get(name)
            .into_iter()
            .flatten()
            .map(|&index| &self.all[index])
    }
    /// The star imports, in source order.
    fn stars(&self) -> impl Iterator<Item = &Import> {
        self.stars.iter().map(|&index| &self.all[index])
    }
}
#[derive(Clone)]
struct Binding {
    candidate: Option<Candidate>,
    public: bool,
    /// `pub(crate)`: visible in its own crate only.
    crate_only: bool,
    /// `pub(self)`, `pub(super)` or `pub(in path)`: the path of the module the item is
    /// visible in, read from the item's own module (#1209).
    restricted: Option<Vec<String>>,
    uncertain: bool,
    /// An `extern crate name;` that keeps the crate's own name (#1166).
    own_crate: bool,
}
impl Binding {
    fn visible(&self, context: ResolveContext<'_, '_>) -> bool {
        self.public && !(self.crate_only && context.foreign)
    }
}
/// How the crate root reads the first segment of a `use` path under edition 2015, where
/// such a path starts at the crate root and not in its own scope (#1166).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RootReading {
    /// Both editions read the path alike: the crate is proved to be of edition 2018 or
    /// later, the `use` is in the root's own scope, or the root binds nothing of that name,
    /// where the 2015 reading would not compile.
    Same,
    /// The root binds the name only by an `extern crate` of that very name: the crate the
    /// edition 2018 reading reaches through the extern prelude when no nearer scope binds it.
    OwnCrate,
    /// Anything else, or a root scope that cannot rule the name out.
    Other,
}
struct Scope {
    document: usize,
    start: usize,
    end: usize,
    parent: Option<usize>,
    module: usize,
    directory: String,
    explicit: BTreeMap<Name, Vec<Binding>>,
    imports: Imports,
    opaque: bool,
    /// Something past the walk bounds was skipped here, so even an explicit name can collide.
    cut: bool,
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
    export_memo: BTreeMap<(usize, usize, bool, bool), std::sync::Arc<ExportMemo>>,
    export_traces: Vec<ExportTrace>,
    /// Whether a nearer binding blocks the extern prelude for a first segment, by the scope
    /// `external_target`'s walk reaches, the segment and strictness (#1210): the walk from
    /// that scope up reads the same scopes for every lookup with no import excluded.
    prelude_memo: BTreeMap<(usize, String, bool), bool>,
    /// A site's own path's names, by the first scope that could bind its first segment and
    /// the path (#1210): every site whose scope climbs to it reads the same answer.
    path_memo: BTreeMap<(usize, Vec<String>), std::sync::Arc<Names>>,
    /// Whether a dependency's statement macro's import reads its package, by the import's
    /// scope and start (#1223), within one pass of `prove_known_macros`.
    import_memo: BTreeMap<(usize, usize), bool>,
    /// Tests read without the two memos above, to compare the answers.
    #[cfg(test)]
    memos: bool,
    files: BTreeMap<usize, usize>,
    file_declarations: BTreeMap<usize, DeclarationId>,
    roots: BTreeMap<usize, BTreeSet<usize>>,
    ambiguous: BTreeSet<usize>,
    /// The extern crate names each crate root scope binds to proved Cargo workspace member
    /// libraries (#772): the library root's file scope, or `None` when bound but unproved.
    externs: BTreeMap<usize, BTreeMap<String, Option<usize>>>,
    /// The crate root scopes whose crate is proved to be of edition 2018 or later, where a
    /// `use` path starts in its own module (#1162). Any other crate may be edition 2015.
    uniform: BTreeSet<usize>,
    /// The items under a known attribute macro whose scope stays transparent until the
    /// macro's path is proved to reach its own package, after the scan.
    proofs: Vec<MacroProof>,
    /// The variants each enum declaration spells, by the declaration's document and range
    /// (#1200).
    variants: BTreeMap<(usize, usize, usize), BTreeMap<String, Variant>>,
    /// What across the checkout could give a statement macro's bare name another meaning
    /// (#1210).
    shadows: MacroShadows,
    /// The statement macro calls, the standard library's (#1210) and dependencies' (#1223),
    /// read as binding nothing until a proof fails.
    statement_macros: Vec<StatementMacro>,
    /// While `scan` is inside an expression statement whose macro calls are all
    /// statement macros: the scope of the outermost such statement.
    statement_owner: Option<usize>,
    pub(super) work: Work,
}
/// A crate root: a conventional root of the checkout's package, or a library or binary
/// root of a proved Cargo workspace member (#772). Its modules live in its directory.
fn crate_root(
    workspace: &super::cargo_workspace::Workspace,
    doc: &DocumentInput,
) -> Option<String> {
    let path = doc.key.path.as_str();
    if matches!(path, "src/lib.rs" | "src/main.rs") {
        return Some("src".to_owned());
    }
    workspace.crate_root(&doc.key.source_set_id, path)?;
    // A root at the checkout's top level (`[lib] path = "lib.rs"`) has the checkout
    // directory, `""`, as its module directory.
    Some(path.rsplit_once('/').map_or("", |(dir, _)| dir).to_owned())
}

impl Catalog {
    pub(super) fn new(
        docs: &[&DocumentInput],
        trees: &[Tree],
        documents: &BTreeMap<DocumentKey, usize>,
        workspace: &super::cargo_workspace::Workspace,
        packages: &external::RustPackages,
        work: Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        let mut result = Self {
            scopes: Vec::new(),
            scope_indexes: BTreeMap::new(),
            export_memo: BTreeMap::new(),
            export_traces: Vec::new(),
            prelude_memo: BTreeMap::new(),
            path_memo: BTreeMap::new(),
            import_memo: BTreeMap::new(),
            #[cfg(test)]
            memos: true,
            files: BTreeMap::new(),
            file_declarations: BTreeMap::new(),
            roots: BTreeMap::new(),
            ambiguous: BTreeSet::new(),
            externs: BTreeMap::new(),
            uniform: BTreeSet::new(),
            proofs: Vec::new(),
            variants: BTreeMap::new(),
            shadows: MacroShadows::default(),
            statement_macros: Vec::new(),
            statement_owner: None,
            work,
        };
        result.read_macro_rules(docs, trees, check)?;
        let declarations = result.index_files(docs, trees, workspace, check)?;
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
        // A parser error can hide an item from any scope of its file, or move it to another.
        for scope in &mut result.scopes {
            if trees[scope.document].root_node().has_error() {
                scope.opaque = true;
                scope.cut = true;
            }
        }
        result.link_module_edges(docs, documents, &mut edges, check)?;
        result.assign_parents(&edges, check)?;
        result.mark_roots(docs, documents, workspace, &edges, check)?;
        result.keep_lost_owners_unknown(docs, trees, &edges, check)?;
        result.prove_known_macros(docs, packages, check)?;
        result.build_scope_indexes(check)?;
        Ok(result)
    }
    /// The declarations of every Rust file by range, and each file's module scope.
    fn index_files<'a>(
        &mut self,
        docs: &[&'a DocumentInput],
        trees: &[Tree],
        workspace: &super::cargo_workspace::Workspace,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<DeclarationsByRange<'a>> {
        let mut declarations = BTreeMap::new();
        for (document, doc) in docs.iter().enumerate() {
            if doc.key.language != "rust" {
                continue;
            }
            for declaration in &doc.declarations {
                self.step(check)?;
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
                self.file_declarations.insert(
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
            let directory = if let Some(directory) = crate_root(workspace, doc) {
                directory
            } else if let Some(prefix) = path.strip_suffix("/mod.rs") {
                prefix.to_owned()
            } else {
                path.strip_suffix(".rs").unwrap_or(path).to_owned()
            };
            let scope = self.add_scope(
                document,
                trees[document].root_node(),
                None,
                directory,
                true,
                check,
            )?;
            self.files.insert(document, scope);
        }
        Ok(declarations)
    }
    /// A `mod name;` finds its file, and the binding of every `mod` learns its module scope.
    fn link_module_edges(
        &mut self,
        docs: &[&DocumentInput],
        documents: &BTreeMap<DocumentKey, usize>,
        edges: &mut [ModuleEdge],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        for edge in edges {
            self.step(check)?;
            if edge.child.is_none() {
                let scope = &self.scopes[edge.scope];
                let doc = docs[scope.document];
                let prefix = child_path(&scope.directory, &edge.name);
                let candidates = find(
                    doc,
                    &[format!("{prefix}.rs"), format!("{prefix}/mod.rs")],
                    documents,
                );
                if let [document] = candidates.as_slice() {
                    edge.child = self.files.get(document).copied();
                }
            }
            if let Some(child) = edge.child {
                let key = (TYPE, edge.name.clone());
                if let Some(bindings) = self.scopes[edge.scope].explicit.get_mut(&key) {
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
        Ok(())
    }
    /// A module with one declaring scope has it as its parent; with several it is ambiguous.
    fn assign_parents(
        &mut self,
        edges: &[ModuleEdge],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut parents: BTreeMap<usize, BTreeSet<usize>> = BTreeMap::new();
        for edge in edges {
            self.step(check)?;
            if let Some(child) = edge.child {
                parents.entry(child).or_default().insert(edge.scope);
            }
        }
        for (child, parents) in parents {
            if parents.len() == 1 {
                self.scopes[child].parent = parents.first().copied();
            } else {
                self.ambiguous.insert(child);
            }
        }
        Ok(())
    }
    /// A file is owned only by a unique, witnessed chain from a conventional root.
    fn mark_roots(
        &mut self,
        docs: &[&DocumentInput],
        documents: &BTreeMap<DocumentKey, usize>,
        workspace: &super::cargo_workspace::Workspace,
        edges: &[ModuleEdge],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        for (&document, &scope) in &self.files.clone() {
            if crate_root(workspace, docs[document]).is_none() {
                continue;
            }
            let key = &docs[document].key;
            self.bind_externs(documents, workspace, key, scope, check)?;
            self.reach_from_root(scope, edges, check)?;
        }
        Ok(())
    }
    /// The extern crate names a workspace member's root binds (#772).
    fn bind_externs(
        &mut self,
        documents: &BTreeMap<DocumentKey, usize>,
        workspace: &super::cargo_workspace::Workspace,
        key: &DocumentKey,
        scope: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        if let Some(root) = workspace.crate_root(&key.source_set_id, &key.path) {
            let mut externs = BTreeMap::new();
            for (name, library) in workspace.externs(&key.source_set_id, root) {
                self.step(check)?;
                let library = library.and_then(|path| {
                    let document = documents.get(&DocumentKey {
                        source_set_id: key.source_set_id.clone(),
                        language: "rust".into(),
                        path: path.to_owned(),
                    })?;
                    self.files.get(document).copied()
                });
                externs.insert(name, library);
            }
            self.externs.insert(scope, externs);
            if workspace.uniform_paths(&key.source_set_id, &key.path) {
                self.uniform.insert(scope);
            }
        }
        Ok(())
    }
    /// Every scope the root `scope` reaches through singly bound `mod` items has it as a root.
    fn reach_from_root(
        &mut self,
        scope: usize,
        edges: &[ModuleEdge],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut pending = vec![(scope, BTreeSet::new())];
        while let Some((current, mut route)) = pending.pop() {
            self.step(check)?;
            if !route.insert(current) {
                continue;
            }
            self.roots.entry(current).or_default().insert(scope);
            for edge in edges {
                self.step(check)?;
                if edge.scope != current {
                    continue;
                }
                let bindings = self.scopes[current]
                    .explicit
                    .get(&(TYPE, edge.name.clone()));
                if bindings.is_some_and(|b| b.len() == 1)
                    && let Some(child) = edge.child
                {
                    pending.push((child, route.clone()));
                }
            }
        }
        Ok(())
    }
    /// One interval index per file over its scopes, for `scope_at`.
    fn build_scope_indexes(&mut self, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
        for id in 0..self.scopes.len() {
            self.step(check)?;
            let scope = &self.scopes[id];
            self.scope_indexes
                .entry(scope.document)
                .or_insert_with(|| ScopeIndex {
                    ranges: Vec::new(),
                    max_ends: Vec::new(),
                })
                .ranges
                .push((id, scope.start, scope.end));
        }
        for index in self.scope_indexes.values_mut() {
            index.ranges.sort_by_key(|&(id, start, _)| (start, id));
            check()?;
            index.max_ends.resize(index.ranges.len(), 0);
            index.build(0, index.ranges.len(), &mut self.work, check)?;
        }
        Ok(())
    }
    /// A parser error can hide a `mod` from its file, and a file with two owners has no proved
    /// crate. So a file a broken module could declare, because its text names it, has no root
    /// unless that broken file still declares it, and neither has anything below it.
    fn keep_lost_owners_unknown(
        &mut self,
        docs: &[&DocumentInput],
        trees: &[Tree],
        edges: &[ModuleEdge],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut pending = Vec::new();
        for (&document, &owner) in &self.files.clone() {
            if !trees[document].root_node().has_error() {
                continue;
            }
            let file = &docs[document].file;
            // A skipped file's placeholder names nothing, but the file could declare any module.
            let (names, unnamed) = match crate::skipped_source::skipped(&file.language, &file.text)
            {
                Some(_) => (BTreeSet::new(), true),
                None => possible_module_names(&file.text),
            };
            let below = match self.scopes[owner].directory.as_str() {
                "" => String::new(),
                directory => format!("{directory}/"),
            };
            for (&other, &child) in &self.files.clone() {
                self.step(check)?;
                let path = &docs[other].key.path;
                let name = path
                    .strip_suffix("/mod.rs")
                    .or_else(|| path.strip_suffix(".rs"))
                    .and_then(|module| module.rsplit('/').next());
                if other != document
                    && path.starts_with(&below)
                    && (unnamed || name.is_some_and(|name| names.contains(name)))
                    && !edges
                        .iter()
                        .any(|edge| edge.scope == owner && edge.child == Some(child))
                {
                    pending.push(child);
                }
            }
        }
        let mut visited = BTreeSet::new();
        while let Some(scope) = pending.pop() {
            self.step(check)?;
            if visited.insert(scope) {
                self.ambiguous.insert(scope);
                pending.extend(
                    edges
                        .iter()
                        .filter(|edge| edge.scope == scope)
                        .filter_map(|edge| edge.child),
                );
            }
        }
        Ok(())
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
        self.charge(self.scopes[scope].imports.all.len(), check)?;
        Ok(self.scopes[scope].imports.all.clone())
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
            imports: Imports::default(),
            opaque: false,
            cut: false,
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
        // A skipped item could bind any name here, as an unknown attribute could.
        if depth > DEPTH {
            self.scopes[scope].opaque = true;
            self.scopes[scope].cut = true;
            return Ok(());
        }
        let document = self.scopes[scope].document;
        let doc = docs[document];
        let mut cursor = node.walk();
        let mut pending = Pending::default();
        let mut scope_uncertain = false;
        for child in node.named_children(&mut cursor) {
            self.step(check)?;
            if matches!(child.kind(), "line_comment" | "block_comment") {
                continue;
            }
            if matches!(child.kind(), "attribute_item" | "inner_attribute_item") {
                self.note_attribute(doc, child, scope, &mut pending, &mut scope_uncertain);
                continue;
            }
            // A `static` or `const` initializer is an expression, which binds no item in
            // the scope holding it (#1191): a macro call there makes only its own scope
            // opaque.
            let scope = if matches!(node.kind(), "const_item" | "static_item")
                && node.child_by_field_name("value") == Some(child)
            {
                let directory = self.scopes[scope].directory.clone();
                self.add_scope(document, child, Some(scope), directory, false, check)?
            } else {
                scope
            };
            let Pending {
                uncertain,
                custom_path: item_custom,
                known,
                ..
            } = std::mem::take(&mut pending);
            let mut item_uncertain = uncertain || scope_uncertain;
            // A known attribute macro keeps the scope transparent only on an item its
            // expansion is read for, and only until its proof fails after the scan.
            let mut proof = None;
            if !known.is_empty() {
                if self.known_macros_fit(doc, child, &known, check)? {
                    proof = Some(MacroProof {
                        scope,
                        macros: known,
                        bindings: Vec::new(),
                        failed: false,
                    });
                } else {
                    self.scopes[scope].opaque = true;
                    item_uncertain = true;
                }
            }
            let item = Item {
                doc,
                document,
                scope,
                child,
                item_uncertain,
                item_custom,
            };
            if child.kind() == "extern_crate_declaration" {
                self.bind_extern_crate(item);
                continue;
            }
            if child.kind() == "use_declaration" {
                self.bind_use(item, check)?;
                continue;
            }
            // A macro call could bind any name in this scope (#1163: the syntax decides),
            // unless it is a statement macro (#1210, #1223), held for its proof.
            if matches!(child.kind(), "macro_invocation" | "expression_statement") {
                match self.macro_calls(doc, child, check)? {
                    MacroCalls::None => {}
                    MacroCalls::Other => {
                        self.scopes[scope].opaque = true;
                        continue;
                    }
                    MacroCalls::Statement if child.kind() == "macro_invocation" => {
                        self.hold_statement_macro(doc, child, scope, check)?;
                        continue;
                    }
                    MacroCalls::Statement => {
                        // Read as any other statement, its nested blocks scopes of their own;
                        // its calls are held as `scan` reaches them, each failed proof making
                        // this scope opaque too, as the statement's macro call did before.
                        let outer = self.statement_owner;
                        self.statement_owner = Some(outer.unwrap_or(scope));
                        self.proofs.extend(proof);
                        let descended = self.descend(docs, item, declarations, edges, depth, check);
                        self.statement_owner = outer;
                        descended?;
                        continue;
                    }
                }
            }
            if opaque(child) {
                continue;
            }
            let namespaces = item_namespaces(child);
            if !namespaces.is_empty()
                && let Some(name) = child.child_by_field_name("name")
            {
                let name = self.bind_item(item, name, namespaces, declarations, check)?;
                if child.kind() == "enum_item" {
                    self.record_variants(doc, document, child, check)?;
                }
                if let Some(proof) = &mut proof {
                    for &namespace in namespaces {
                        let key = (namespace, name.clone());
                        let at = self.scopes[scope].explicit[&key].len() - 1;
                        proof.bindings.push((key, at));
                    }
                }
                if child.kind() == "mod_item" {
                    self.enter_module(docs, item, name, declarations, edges, depth, check)?;
                    continue;
                }
            }
            self.proofs.extend(proof);
            self.descend(docs, item, declarations, edges, depth, check)?;
        }
        Ok(())
    }
    /// Which macro calls lie anywhere below a statement, nested blocks included: none, only
    /// statement macros (#1210, #1223), or another. Every macro call spells `!`,
    /// so a statement without one needs no walk; `!=`, a unary `!` and a `!` in a literal do.
    /// A call's own arguments are tokens, not read.
    fn macro_calls(
        &mut self,
        doc: &DocumentInput,
        statement: Node<'_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<MacroCalls> {
        if !text(doc, statement).contains('!') {
            return Ok(MacroCalls::None);
        }
        let mut found = MacroCalls::None;
        let mut pending = vec![statement];
        while let Some(node) = pending.pop() {
            self.step(check)?;
            if node.kind() == "macro_invocation" {
                if statement_macro(doc, node).is_none() {
                    return Ok(MacroCalls::Other);
                }
                found = MacroCalls::Statement;
                continue;
            }
            let mut cursor = node.walk();
            pending.extend(node.named_children(&mut cursor));
        }
        Ok(found)
    }
    /// Holds a statement macro call in `scope` for its proof. A
    /// `thread_local!` declares each `static` it spells, as a constant this catalog does not
    /// follow: an uncertain value binding.
    fn hold_statement_macro(
        &mut self,
        doc: &DocumentInput,
        call: Node<'_>,
        scope: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        self.step(check)?;
        let Some((source, path, global)) = statement_macro(doc, call) else {
            self.scopes[scope].opaque = true;
            return Ok(());
        };
        if path.last().is_some_and(|name| name == "thread_local") {
            let mut cursor = call.walk();
            let arguments = call
                .named_children(&mut cursor)
                .find(|node| node.kind() == "token_tree");
            let tokens: Vec<Node<'_>> = arguments
                .map(|node| node.children(&mut node.walk()).collect())
                .unwrap_or_default();
            for (at, token) in tokens.iter().enumerate() {
                self.step(check)?;
                // `static NAME:`, and not the `static` of a `'static` lifetime.
                let declares = at >= 1
                    && token.kind() == "identifier"
                    && text(doc, tokens[at - 1]) == "static"
                    && (at < 2 || text(doc, tokens[at - 2]) != "'");
                if declares {
                    self.scopes[scope]
                        .explicit
                        .entry((VALUE, text(doc, *token).to_owned()))
                        .or_default()
                        .push(Binding {
                            candidate: None,
                            public: true,
                            crate_only: false,
                            restricted: None,
                            uncertain: true,
                            own_crate: false,
                        });
                }
            }
        }
        self.statement_macros.push(StatementMacro {
            scope,
            owner: self.statement_owner.unwrap_or(scope),
            source,
            path,
            global,
            settled: false,
            failed: false,
        });
        Ok(())
    }
    /// Reads, from every Rust file's text, each name a `macro_rules!` definition could take
    /// (#1210). The reading is textual, so a definition inside another macro's arguments, a
    /// comment or a string counts too; a file whose text was not read could define any.
    fn read_macro_rules(
        &mut self,
        docs: &[&DocumentInput],
        trees: &[Tree],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        for (document, doc) in docs.iter().enumerate() {
            if doc.key.language != "rust" {
                continue;
            }
            self.step(check)?;
            let text = doc.file.text.as_str();
            if crate::skipped_source::skipped(&doc.file.language, text).is_some() {
                self.shadows.unread = true;
                continue;
            }
            for name in macro_rules_names(text) {
                self.step(check)?;
                self.shadows.rules.insert(name);
            }
            // An attribute a parser error hides is read from the text instead.
            if trees[document].root_node().has_error()
                && (text.contains("macro_use") || text.contains("decl_macro"))
            {
                self.shadows.unread = true;
            }
        }
        Ok(())
    }
    /// What an attribute says about the item after it, or with `#!` about the whole scope.
    /// An attribute this catalog does not read could be a macro that binds any name, so it
    /// makes the scope opaque. A built-in inert attribute cannot be a macro: a macro of the
    /// same name is an ambiguity error. A derive helper is inert only after a derive on the
    /// same item that registers it; before that, or without one, the name is an ordinary
    /// attribute macro. A known attribute macro (`KnownMacro`) is held for the item after it
    /// instead: `scan` checks the item and `prove_known_macros` the macro's package.
    fn note_attribute(
        &mut self,
        doc: &DocumentInput,
        child: Node<'_>,
        scope: usize,
        pending: &mut Pending,
        scope_uncertain: &mut bool,
    ) {
        let attribute: String = text(doc, child)
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let inner = child.kind() == "inner_attribute_item";
        let name = attribute_name(&attribute);
        // Either could give a statement macro's bare name another meaning
        // (#1210): `macro_use`, and the `decl_macro` feature, which allows a `macro` item.
        self.shadows.macro_use |= name == "macro_use";
        self.shadows.decl_macro |= inner && spells_identifier(&attribute, "decl_macro");
        if attribute.starts_with("#![cfg(") || attribute.starts_with("#![cfg_attr(") {
            *scope_uncertain = true;
        } else if attribute.starts_with("#[cfg(")
            || attribute.starts_with("#[cfg_attr(")
            || attribute == "#[test]"
        {
            pending.uncertain = true;
        } else if attribute.starts_with("#[path=") {
            pending.custom_path = true;
        } else if !inner && attribute.starts_with("#[derive(") {
            if let Some(list) = attribute
                .strip_prefix("#[derive(")
                .and_then(|rest| rest.strip_suffix(")]"))
            {
                for derive in list.split(',') {
                    let derive = derive.rsplit("::").next().unwrap_or(derive);
                    pending.helpers.extend(derive_helpers(derive));
                }
            }
        } else if !inner && let Some(known) = known_macro(name) {
            // tokio-macros adds `#[::core::prelude::v1::test]` to a `#[tokio::test]`
            // function, which then exists in test builds only, as under `#[test]`.
            if matches!(known, KnownMacro::Tokio { test: true, .. }) {
                pending.uncertain = true;
            }
            pending.known.push(known);
        } else if !(INERT_ATTRIBUTES.contains(&name)
            || name.starts_with("rustfmt::")
            || name.starts_with("clippy::")
            || (!inner && pending.helpers.contains(name)))
        {
            self.scopes[scope].opaque = true;
            if inner {
                *scope_uncertain = true;
            } else {
                pending.uncertain = true;
            }
        }
    }
    /// Whether the item after known attribute macros is one their expansions are read for:
    /// for tokio a function whose body has no inner attribute, for async-trait a trait or a
    /// trait impl that spells no `__self`.
    fn known_macros_fit(
        &mut self,
        doc: &DocumentInput,
        child: Node<'_>,
        macros: &[KnownMacro],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        // Each shape is read once per item, however many attributes ask for it; the walk
        // over the body's statements is charged per statement.
        let tokio = macros
            .iter()
            .any(|known| matches!(known, KnownMacro::Tokio { .. }));
        let async_trait = macros
            .iter()
            .any(|known| matches!(known, KnownMacro::AsyncTrait { .. }));
        self.charge(macros.len(), check)?;
        if tokio {
            if child.kind() != "function_item" {
                return Ok(false);
            }
            let Some(body) = child.child_by_field_name("body") else {
                return Ok(false);
            };
            let mut cursor = body.walk();
            for statement in body.named_children(&mut cursor) {
                self.step(check)?;
                if statement.kind() == "inner_attribute_item" {
                    return Ok(false);
                }
            }
        }
        if async_trait
            && !((child.kind() == "trait_item"
                || (child.kind() == "impl_item" && child.child_by_field_name("trait").is_some()))
                && !spells_identifier(text(doc, child), "__self"))
        {
            return Ok(false);
        }
        Ok(true)
    }
    /// Proves each known attribute macro's path reaches its own package, and each held
    /// statement macro call reaches its source's (#1210, #1223). An item whose proof
    /// fails is read as under an attribute this catalog does not read: its scope opaque and
    /// its names uncertain; a failed call makes its own scope and its outermost statement's
    /// opaque, as any other macro call. That can change how another proof reads (a glob over
    /// the now opaque scope), so the proofs are rechecked until none fails; each scope only
    /// turns opaque, so this ends, and every step is charged to the work budget. The memos
    /// read the scopes before they turned opaque and are cleared after each pass.
    fn prove_known_macros(
        &mut self,
        docs: &[&DocumentInput],
        packages: &external::RustPackages,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        loop {
            let mut failed = false;
            for index in 0..self.proofs.len() {
                self.step(check)?;
                if self.proofs[index].failed {
                    continue;
                }
                let scope = self.proofs[index].scope;
                let mut proved = true;
                for known in self.proofs[index].macros.clone() {
                    if !self.proves_known_macro(scope, known, docs, packages, check)? {
                        proved = false;
                        break;
                    }
                }
                if !proved {
                    failed = true;
                    let proof = &mut self.proofs[index];
                    proof.failed = true;
                    let bindings = std::mem::take(&mut proof.bindings);
                    self.scopes[scope].opaque = true;
                    for (key, at) in bindings {
                        self.step(check)?;
                        if let Some(binding) = self.scopes[scope]
                            .explicit
                            .get_mut(&key)
                            .and_then(|bindings| bindings.get_mut(at))
                        {
                            binding.uncertain = true;
                        }
                    }
                }
            }
            for index in 0..self.statement_macros.len() {
                self.step(check)?;
                let held = &self.statement_macros[index];
                if held.failed || held.settled {
                    continue;
                }
                let (scope, owner) = (held.scope, held.owner);
                if !self.proves_statement_macro(index, docs, packages, check)? {
                    failed = true;
                    self.statement_macros[index].failed = true;
                    self.scopes[scope].opaque = true;
                    self.scopes[owner].opaque = true;
                }
            }
            self.forget_reads();
            if !failed {
                return Ok(());
            }
        }
    }
    /// Drops every memo of what the scopes read, once a scope has changed.
    fn forget_reads(&mut self) {
        self.export_memo.clear();
        self.prelude_memo.clear();
        self.path_memo.clear();
        self.import_memo.clear();
    }
    fn proves_known_macro(
        &mut self,
        scope: usize,
        known: KnownMacro,
        docs: &[&DocumentInput],
        packages: &external::RustPackages,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        let path = |a: &str, b: &str| [a.to_owned(), b.to_owned()];
        match known {
            KnownMacro::Tokio { test, global } => {
                let name = if test { "test" } else { "main" };
                let path = path("tokio", name);
                self.proves_package(scope, &path, global, None, "tokio", docs, packages, check)
            }
            KnownMacro::AsyncTrait {
                bare: false,
                global,
            } => {
                let path = path("async_trait", "async_trait");
                self.proves_package(
                    scope,
                    &path,
                    global,
                    None,
                    "async_trait",
                    docs,
                    packages,
                    check,
                )
            }
            KnownMacro::AsyncTrait { bare: true, .. } => {
                // The nearest scope that binds `async_trait` must do so by exactly one
                // certain `use async_trait::async_trait;`: no item, glob or opaque scope
                // before it, whose binding a re-export (`use axum::async_trait;`) or another
                // macro could be.
                let mut current = Some(scope);
                while let Some(at) = current {
                    self.step(check)?;
                    let key = |namespace| (namespace, "async_trait".to_owned());
                    if self.scopes[at].opaque
                        || self.scopes[at].explicit.contains_key(&key(TYPE))
                        || self.scopes[at].explicit.contains_key(&key(VALUE))
                    {
                        return Ok(false);
                    }
                    let (import, more) = {
                        let mut named = self.scopes[at].imports.named("async_trait");
                        (named.next().cloned(), named.next().is_some())
                    };
                    if let Some(import) = import {
                        if more
                            || import.uncertain
                            || import.path != path("async_trait", "async_trait")
                        {
                            return Ok(false);
                        }
                        return self.proves_package(
                            at,
                            &import.path,
                            false,
                            Some((import.start, import.end)),
                            "async_trait",
                            docs,
                            packages,
                            check,
                        );
                    }
                    if self.scopes[at].imports.stars().next().is_some() {
                        return Ok(false);
                    }
                    current = if at == self.scopes[at].module {
                        None
                    } else {
                        self.scopes[at].parent
                    };
                }
                Ok(false)
            }
        }
    }
    /// Whether the first segment of `path`, read from `scope`, is the Cargo dependency of the
    /// package `package`, as `proved_dependency` proves it.
    #[allow(clippy::too_many_arguments)] // The proof's scope, path and captured context.
    fn proves_package(
        &mut self,
        scope: usize,
        path: &[String],
        global: bool,
        excluding: Option<(usize, usize)>,
        package: &str,
        docs: &[&DocumentInput],
        packages: &external::RustPackages,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        let target =
            self.proved_dependency(scope, path, global, excluding, docs, packages, check)?;
        // crates.io treats `-` and `_` in a package name as the same name.
        Ok(target.is_some_and(|target| {
            !target.identity.standard_library
                && target.identity.package.replace('-', "_") == package
        }))
    }
    /// The package the first segment of `path`, read from `scope`, is proved to name:
    /// `external_target`, strictly, finds no nearer item, import or glob that binds it or
    /// could (a glob over another dependency could export a module of that name), and the
    /// crate root has no `extern crate .. as` of that name, which would take its place in the
    /// extern prelude. A manifest dependency of that name is the package it binds, and `std`,
    /// `core` or `alloc` otherwise the standard library. `global` is a leading `::`, which
    /// edition 2015 reads from the crate root. `excluding` is the import whose own path this
    /// is.
    #[allow(clippy::too_many_arguments)] // The proof's scope, path and captured context.
    fn proved_dependency(
        &mut self,
        scope: usize,
        path: &[String],
        global: bool,
        excluding: Option<(usize, usize)>,
        docs: &[&DocumentInput],
        packages: &external::RustPackages,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<external::ExternalPackage>> {
        self.step(check)?;
        let Some(root) = self.root(scope) else {
            return Ok(None);
        };
        let first = (TYPE, path[0].clone());
        if self.scopes[root].cut
            || self.scopes[root]
                .explicit
                .get(&first)
                .is_some_and(|bindings| bindings.iter().any(|b| !b.own_crate))
            || (global && self.root_reading(scope, &path[0], check)? == RootReading::Other)
        {
            return Ok(None);
        }
        self.external_target(scope, path, packages, docs, excluding, true, check)
    }
    /// Whether a held statement macro call reaches its source's macro (#1210), by the
    /// source and the path written. A proof that reads nothing a failed proof changes marks
    /// the call settled, proved once.
    fn proves_statement_macro(
        &mut self,
        index: usize,
        docs: &[&DocumentInput],
        packages: &external::RustPackages,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        let held = &self.statement_macros[index];
        let (scope, global, source) = (held.scope, held.global, held.source);
        let path = held.path.clone();
        match (source, path.as_slice()) {
            (MacroSource::StandardLibrary, [name]) => {
                // Read from imports and facts no pass changes.
                let proved = self.proves_bare_statement_macro(scope, name, check)?;
                self.statement_macros[index].settled = proved;
                Ok(proved)
            }
            (MacroSource::StandardLibrary, _) => Ok(self
                .proved_dependency(scope, &path, global, None, docs, packages, check)?
                .is_some_and(|target| target.identity.standard_library)),
            // `proves_package` reads which scopes are opaque, so these are rechecked each pass.
            (MacroSource::Package(package), [name]) => {
                self.proves_bare_package_macro(scope, package, name, docs, packages, check)
            }
            (MacroSource::Package(package), _) => {
                self.proves_package(scope, &path, global, None, package, docs, packages, check)
            }
        }
    }
    /// Whether a dependency's statement macro's bare name, called in `scope`, reaches the
    /// package's macro (#1223). Nothing checkout-wide may shadow the name, as for a
    /// standard-library macro, and the nearest scope from the call's up to its module that
    /// imports the name by name must do so by exactly one certain `use package::name;`,
    /// proved to read the package. With none, one glob in those scopes over an ancestor
    /// module (`super::*`, `super::super::*`, `crate::*`) may bring in that module's own
    /// such import. On a checkout that compiles nothing else can take the name's place: a
    /// macro of the name that another glob, an outer scope's import or an import another
    /// macro's expansion writes brings in is ambiguous (E0659), and a `macro_rules!` that
    /// another macro's expansion defines does not shadow the import (rustc probes on editions
    /// 2021 and 2024, recorded with #1223). A chain of two globs or more is not followed.
    fn proves_bare_package_macro(
        &mut self,
        scope: usize,
        package: &str,
        name: &str,
        docs: &[&DocumentInput],
        packages: &external::RustPackages,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        if self.shadows.shadow(name) {
            return Ok(false);
        }
        let wanted = [package.to_owned(), name.to_owned()];
        let mut chain = Vec::new();
        let mut current = Some(scope);
        while let Some(at) = current {
            self.step(check)?;
            if self.scopes[at].cut {
                return Ok(false);
            }
            if self.scopes[at].imports.named(name).next().is_some() {
                return match self.sole_import(at, name, &wanted) {
                    Some(import) => {
                        self.proves_import(at, import, &wanted, package, docs, packages, check)
                    }
                    None => Ok(false),
                };
            }
            chain.push(at);
            current = if at == self.scopes[at].module {
                None
            } else {
                self.scopes[at].parent
            };
        }
        for at in chain {
            let globs: Vec<Vec<String>> = (self.scopes[at].imports.stars())
                .filter(|star| !star.uncertain)
                .map(|star| star.path.clone())
                .collect();
            for glob in globs {
                self.step(check)?;
                let Some(module) = self.ancestor_module(at, &glob) else {
                    continue;
                };
                if self.scopes[module].cut {
                    continue;
                }
                if let Some(import) = self.sole_import(module, name, &wanted) {
                    return self
                        .proves_import(module, import, &wanted, package, docs, packages, check);
                }
            }
        }
        Ok(false)
    }
    /// The range of scope `at`'s named import of `name` when it is the only one, certain,
    /// and imports `wanted`.
    fn sole_import(&self, at: usize, name: &str, wanted: &[String]) -> Option<(usize, usize)> {
        let mut named = self.scopes[at].imports.named(name);
        let import = named.next()?;
        (named.next().is_none() && !import.uncertain && import.path == wanted)
            .then_some((import.start, import.end))
    }
    /// Whether the import of `path` at `import` in scope `at` reads `package`, by
    /// `proves_package`. Every call through one import reads the same answer, so within a
    /// pass of `prove_known_macros` it is remembered; a hit is charged its step.
    #[allow(clippy::too_many_arguments)] // As `proves_package`.
    fn proves_import(
        &mut self,
        at: usize,
        import: (usize, usize),
        path: &[String],
        package: &str,
        docs: &[&DocumentInput],
        packages: &external::RustPackages,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        self.step(check)?;
        let key = (at, import.0);
        if self.memos()
            && let Some(&proved) = self.import_memo.get(&key)
        {
            return Ok(proved);
        }
        let proved = self.proves_package(
            at,
            path,
            false,
            Some(import),
            package,
            docs,
            packages,
            check,
        )?;
        self.import_memo.insert(key, proved);
        Ok(proved)
    }
    /// The module a glob's path names when it is an ancestor of `scope`'s module, read as
    /// `path` reads it: `crate`, or one or more `super`, in a module of one proved root.
    fn ancestor_module(&self, scope: usize, path: &[String]) -> Option<usize> {
        let root = self.root(scope)?;
        if path == ["crate"] {
            return Some(root);
        }
        if path.is_empty() || path.iter().any(|segment| segment != "super") {
            return None;
        }
        let mut at = self.scopes[scope].module;
        for _ in path {
            at = self.parent_module(at)?;
        }
        Some(at)
    }
    /// Whether a standard-library statement macro's bare name, called in `scope`, reaches
    /// the standard library's macro (#1210). On a checkout that compiles, only a literal
    /// `macro_rules!` of the name, `#[macro_use]`, a `macro` item (the `decl_macro` feature)
    /// or a named import of it can silently take its place; a glob import, or an import that
    /// a macro's expansion writes, is an ambiguity error (E0659), and a `macro_rules!` that
    /// another macro's expansion defines does not shadow it. So: no `macro_rules!` anywhere
    /// could define the name, no `#[macro_use]` or `decl_macro` is read, every file was
    /// read, and no scope from the call's up to its module has a named import of it or
    /// skipped anything.
    fn proves_bare_statement_macro(
        &mut self,
        scope: usize,
        name: &str,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        if self.shadows.shadow(name) {
            return Ok(false);
        }
        let mut current = Some(scope);
        while let Some(at) = current {
            self.step(check)?;
            if self.scopes[at].cut || self.scopes[at].imports.named(name).next().is_some() {
                return Ok(false);
            }
            current = if at == self.scopes[at].module {
                None
            } else {
                self.scopes[at].parent
            };
        }
        Ok(true)
    }
    /// `extern crate name;` binds a name this catalog does not follow.
    fn bind_extern_crate(&mut self, item: Item<'_, '_>) {
        let Item {
            doc, scope, child, ..
        } = item;
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
                    crate_only: false,
                    restricted: None,
                    uncertain: true,
                    own_crate: child.child_by_field_name("alias").is_none(),
                });
        }
    }
    /// The imports of one `use` declaration, indexed in their scope.
    fn bind_use(
        &mut self,
        item: Item<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let Item {
            doc,
            scope,
            child,
            item_uncertain,
            ..
        } = item;
        let public = visibility(doc, child).is_some();
        let crate_only = crate_visibility(doc, child);
        let restricted = visibility(doc, child).is_some_and(|v| v != "pub") && !crate_only;
        if let Some(argument) = child.child_by_field_name("argument") {
            let (imports, cut) = imports(
                doc,
                argument,
                (public, crate_only),
                item_uncertain || restricted,
                &mut self.work,
                check,
            )?;
            // Indexed once here; a name lookup then reads only its matches (#894).
            self.charge(imports.len(), check)?;
            for import in imports {
                self.scopes[scope].imports.push(import);
            }
            // A skipped import could bind any name, so nothing here is proved absent.
            self.scopes[scope].opaque |= cut;
            self.scopes[scope].cut |= cut;
        }
        Ok(())
    }
    /// Binds a named item in each of its namespaces and returns its name.
    fn bind_item(
        &mut self,
        item: Item<'_, '_>,
        name: Node<'_>,
        namespaces: &[u8],
        declarations: &BTreeMap<(usize, usize, usize), Vec<&Declaration>>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<String> {
        let Item {
            doc,
            document,
            scope,
            child,
            item_uncertain,
            item_custom,
        } = item;
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
        // `pub(crate)` is visible throughout its crate (#828); another restricted visibility
        // is visible in the module its path names (#1209), and one whose path is not read
        // is not modeled.
        let crate_only = crate_visibility(doc, child);
        let restricted = visibility(doc, child)
            .filter(|v| *v != "pub" && !crate_only)
            .map(restriction_path);
        let unread = restricted.as_ref().is_some_and(Option::is_none);
        for &namespace in namespaces {
            self.step(check)?;
            self.scopes[scope]
                .explicit
                .entry((namespace, name.clone()))
                .or_default()
                .push(Binding {
                    candidate: candidate.clone(),
                    public,
                    crate_only,
                    restricted: restricted.clone().flatten(),
                    uncertain: item_uncertain
                        || unread
                        || item_custom
                        || declaration.is_some_and(|d| d.role == DeclarationRole::Test),
                    own_crate: false,
                });
        }
        Ok(name)
    }
    /// An inline `mod` gets its scope and is scanned; every `mod` without a custom path is an edge.
    #[allow(clippy::too_many_arguments)] // Shares the same captured scan inputs and depth budget.
    fn enter_module(
        &mut self,
        docs: &[&DocumentInput],
        item: Item<'_, '_>,
        name: String,
        declarations: &BTreeMap<(usize, usize, usize), Vec<&Declaration>>,
        edges: &mut Vec<ModuleEdge>,
        depth: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let Item {
            document,
            scope,
            child,
            item_custom,
            ..
        } = item;
        let directory = child_path(&self.scopes[scope].directory, &name);
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
        Ok(())
    }
    /// The scopes below an item that is not a module: the bodies of a type's members, a block,
    /// or whatever the node holds.
    fn descend(
        &mut self,
        docs: &[&DocumentInput],
        item: Item<'_, '_>,
        declarations: &BTreeMap<(usize, usize, usize), Vec<&Declaration>>,
        edges: &mut Vec<ModuleEdge>,
        depth: usize,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let Item {
            document,
            scope,
            child,
            ..
        } = item;
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
            if depth > DEPTH {
                self.scopes[scope].opaque = true;
                self.scopes[scope].cut = true;
                break;
            }
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
                                crate_only: false,
                                restricted: None,
                                uncertain: true,
                                own_crate: false,
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
        // A skipped item could bind any name here, as an unknown attribute could.
        if depth > DEPTH {
            self.scopes[scope].opaque = true;
            self.scopes[scope].cut = true;
            return Ok(());
        }
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
    /// The names `scope` binds as `name`. `exported` says the lookup comes from outside the
    /// module of `scope`, from `site` when that is known: an export set has no one site.
    #[allow(clippy::too_many_arguments)] // Captured context and route remain explicit.
    fn binding(
        &mut self,
        scope: usize,
        name: &str,
        exported: bool,
        site: Option<usize>,
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
                if exported && !binding.visible(context) {
                    continue;
                }
                let hidden = match &binding.restricted {
                    Some(path) if exported => !self.restriction_admits(scope, path, site, check)?,
                    _ => false,
                };
                if binding.uncertain || hidden {
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
        // Only the imports that can bind `name`: its named imports and every star (#894).
        let named: Vec<Import> = self.scopes[scope].imports.named(name).cloned().collect();
        let stars: Vec<Import> = self.scopes[scope].imports.stars().cloned().collect();
        self.charge(named.len() + stars.len(), check)?;
        for import in &named {
            let mut resolved = if import.uncertain {
                Names {
                    unknown_set: true,
                    ..Names::default()
                }
            } else {
                // The import's own path resolves in its defining crate; whether the caller
                // may see the import is checked against the caller's context below.
                self.path(
                    scope,
                    &import.path,
                    true,
                    depth + 1,
                    route,
                    ResolveContext {
                        foreign: false,
                        ..context
                    },
                    check,
                )?
            };
            if resolved.unknown_set && (!exported || import.visible(context)) {
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
                if exported && !import.visible(context) {
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
        let mut star_names = Names::default();
        for import in stars.iter().filter(|i| !exported || i.visible(context)) {
            if self.waived_star(scope, import, context, check)? {
                continue;
            }
            let expanded =
                self.expand(scope, import, depth + 1, route, context, Some(name), check)?;
            star_names.unknown_set |= expanded.unknown_set;
            for (key, bucket) in expanded
                .names
                .into_iter()
                .filter(|((_, spelling), _)| spelling == name)
            {
                self.merge_bucket(star_names.names.entry(key).or_default(), bucket, check)?;
            }
        }
        for (key, mut bucket) in star_names.names {
            if explicit_namespaces.contains(&key.0) {
                continue;
            }
            bucket.unknown |= star_names.unknown_set;
            result.names.insert(key, bucket);
        }
        if star_names.unknown_set {
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
                if self.scopes[scope].cut || !explicit_namespaces.contains(&namespace) {
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
    /// Whether a restricted item of `declaring`'s module, visible in the module `path` names
    /// (`pub(in path)`, with `pub(self)` and `pub(super)` read as `pub(in self)` and
    /// `pub(in super)`), is visible at `site` (#1209). A lookup that has private access to
    /// the item's module needs no proof; this is for one from outside it. The path is read
    /// from the item's module: `crate`, `self` or `super`s, then the names of single `mod`
    /// items down to the module the item is visible in, which must be the item's module or
    /// one of its ancestors, as the compiler requires. The site is inside when its own
    /// module is that module or below it. Anything else, a lookup with no one site
    /// included, is not proved.
    fn restriction_admits(
        &mut self,
        declaring: usize,
        path: &[String],
        site: Option<usize>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        self.step(check)?;
        let Some(site) = site else {
            return Ok(false);
        };
        let module = self.scopes[declaring].module;
        let (mut current, mut rest) = match path.split_first() {
            Some((first, rest)) if first == "crate" => match self.root(module) {
                Some(root) => (root, rest),
                None => return Ok(false),
            },
            Some((first, rest)) if first == "self" => (module, rest),
            _ => (module, path),
        };
        while let Some((first, tail)) = rest.split_first()
            && first == "super"
        {
            let Some(parent) = self.parent_module(current) else {
                return Ok(false);
            };
            current = parent;
            rest = tail;
        }
        for name in rest {
            self.step(check)?;
            let Some([binding]) = self.scopes[current]
                .explicit
                .get(&(TYPE, name.clone()))
                .map(Vec::as_slice)
            else {
                return Ok(false);
            };
            let Some(child) = binding.candidate.as_ref().and_then(|c| c.module) else {
                return Ok(false);
            };
            current = child;
        }
        Ok(self.private_access(module, current, check)?
            && self.private_access(site, current, check)?)
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
    /// How the crate root of `scope` reads `first`, the first segment of a `use` path in
    /// `scope`, under edition 2015 (#1166). Only a crate whose manifest proves edition 2018
    /// or later is read the 2018 way alone. The root binds the name by an item, `extern
    /// crate` included, by a named import or possibly by a glob, or, when it cannot be read
    /// whole, by anything. The root's `std` and `core` are the crates of those names, as the
    /// `extern crate` the compiler adds to a 2015 crate root binds them, even in a root that
    /// cannot be read whole: a macro there defining another item of either name is not
    /// detected.
    fn root_reading(
        &mut self,
        scope: usize,
        first: &str,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<RootReading> {
        self.step(check)?;
        let Some(root) = self.root(scope) else {
            return Ok(RootReading::Same);
        };
        if scope == root || self.uniform.contains(&root) {
            return Ok(RootReading::Same);
        }
        let types = (TYPE, first.to_owned());
        let values = (VALUE, first.to_owned());
        let root_scope = &self.scopes[root];
        if root_scope.explicit.contains_key(&types) || root_scope.explicit.contains_key(&values) {
            let bindings = root_scope.explicit.get(&types).map_or(0, Vec::len);
            let own = !root_scope.cut
                && !root_scope.explicit.contains_key(&values)
                && root_scope.explicit[&types].iter().all(|b| b.own_crate);
            self.charge(bindings, check)?;
            return Ok(if own {
                RootReading::OwnCrate
            } else {
                RootReading::Other
            });
        }
        if root_scope.imports.named(first).next().is_some()
            || root_scope.imports.stars().next().is_some()
        {
            return Ok(RootReading::Other);
        }
        if matches!(first, "std" | "core") {
            return Ok(RootReading::OwnCrate);
        }
        Ok(if root_scope.opaque {
            RootReading::Other
        } else {
            RootReading::Same
        })
    }
    /// The names `path` resolves to from `scope`. `import` says it is the path of a `use`
    /// item, which edition 2015 starts at the crate root (#1166).
    #[allow(clippy::too_many_arguments)] // The lookup's captured context and route, as `binding`.
    fn path(
        &mut self,
        scope: usize,
        path: &[String],
        import: bool,
        depth: usize,
        route: &mut Route,
        mut context: ResolveContext<'_, '_>,
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
        let mut crate_root = self.root(scope).expect("proved root");
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
        // A 2015 `use` path starts at the crate root: where the 2018 reading finds its first
        // segment elsewhere and the root binds it too, the two editions can differ.
        let reading = if import && lexical {
            self.root_reading(scope, &path[0], check)?
        } else {
            RootReading::Same
        };
        if index == path.len() {
            return Ok(self.module_name(current, &path[index - 1], depth));
        }
        let mut proof = BTreeSet::new();
        loop {
            self.step(check)?;
            proof.insert(self.scopes[current].document);
            let exported = current == self.scopes[current].module
                && !self.private_access(scope, current, check)?;
            let declared = self.declared_module(
                current,
                &path[index..],
                exported,
                scope,
                depth,
                route,
                context,
                check,
            )?;
            let mut found = match declared {
                Some(found) => found,
                None => self.binding(
                    current,
                    &path[index],
                    exported,
                    Some(scope),
                    depth,
                    route,
                    context,
                    check,
                )?,
            };
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
            // An unbound first segment can name a workspace member crate the importing crate
            // depends on (#772); local names shadow it, as in the extern prelude.
            if lexical
                && index == 0
                && !found.unknown_set
                && found
                    .names
                    .values()
                    .all(|b| b.candidates.is_empty() && !b.unknown)
                && let Some(library) = self
                    .externs
                    .get(&crate_root)
                    .and_then(|externs| externs.get(&path[0]))
                    .copied()
            {
                // Under edition 2015 the root's own binding of the name answers instead.
                let Some(library) =
                    library.filter(|_| reading != RootReading::Other || current == crate_root)
                else {
                    return Ok(Names {
                        unknown_set: true,
                        ..Names::default()
                    });
                };
                lexical = false;
                context.foreign = true;
                crate_root = library;
                current = library;
                index = 1;
                if index == path.len() {
                    return Ok(self.module_name(current, &path[0], depth));
                }
                continue;
            }
            // The 2018 reading found the first segment in a scope below the crate root, where
            // the 2015 one takes the root's binding.
            if lexical && reading != RootReading::Same && current != crate_root {
                return Ok(Names {
                    unknown_set: true,
                    ..Names::default()
                });
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
                // Only an enum's variant (#1200) goes on from a candidate that is no module.
                let enumeration = next.filter(|_| !found.unknown_set).cloned();
                return self.variant(enumeration.as_ref(), &path[index..], check);
            }
            let next = next.expect("proved module candidate");
            proof.extend(next.proof.iter().copied());
            depth = depth.max(next.follow);
            current = next.module.expect("proved module");
            // A proved binding (such as `use member::model as alias;`) can name a module of
            // another workspace member: the rest of the path continues in that crate, with
            // its foreign visibility.
            match self.root(current) {
                Some(root) if root == crate_root => {}
                Some(root) => {
                    crate_root = root;
                    context.foreign = self.root(scope) != Some(root);
                }
                None => {
                    return Ok(Names {
                        unknown_set: true,
                        ..Names::default()
                    });
                }
            }
        }
    }
    /// The module a path prefix names when the lookup of that same name in that same scope is
    /// already under way: `mod parse; pub use parse::parse;` asks for the prefix `parse`
    /// while binding `parse`. Only a module continues a path, and from edition 2018 a `use`
    /// path starts in its own scope, whose `mod` item is that module whatever the import
    /// under way binds, so the prefix is proved without the import. In edition 2015 the
    /// same `use` path starts at the crate root and can name another module. So only a
    /// single unconditional `mod`, in a scope read whole, of a crate whose manifest proves
    /// edition 2018 or later qualifies; for anything else `binding` answers the re-entry
    /// with unknown.
    #[allow(clippy::too_many_arguments)] // The lookup's captured context and route, as `binding`.
    fn declared_module(
        &mut self,
        scope: usize,
        rest: &[String],
        exported: bool,
        site: usize,
        depth: usize,
        route: &Route,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Names>> {
        // A prefix has a segment after it; the last segment is always `binding`'s.
        let [name, _, ..] = rest else {
            return Ok(None);
        };
        if !route.contains(&(scope, name.clone())) {
            return Ok(None);
        }
        self.step(check)?;
        let key = (TYPE, name.clone());
        let declaring = &self.scopes[scope];
        let Some([binding]) = declaring.explicit.get(&key).map(Vec::as_slice) else {
            return Ok(None);
        };
        let restricted = binding.restricted.clone();
        if exported
            && let Some(path) = &restricted
            && !self.restriction_admits(scope, path, Some(site), check)?
        {
            return Ok(None);
        }
        let declaring = &self.scopes[scope];
        let binding = &declaring.explicit[&key][0];
        if declaring.cut
            || binding.uncertain
            || (exported && !binding.visible(context))
            || !self
                .root(scope)
                .is_some_and(|root| self.uniform.contains(&root))
        {
            return Ok(None);
        }
        let Some(mut candidate) = binding.candidate.clone().filter(|c| c.module.is_some()) else {
            return Ok(None);
        };
        candidate.follow = candidate.follow.max(depth);
        candidate.proof.insert(declaring.document);
        // The answer depends on the route, as a refused re-entry does: no export set
        // computed around it is cached.
        self.charge(self.export_traces.len(), check)?;
        for trace in &mut self.export_traces {
            trace.consulted.insert((scope, name.clone()));
            trace.cycle = true;
        }
        let mut names = Names::default();
        names
            .names
            .entry(key)
            .or_default()
            .candidates
            .insert(candidate.declaration.clone(), candidate);
        Ok(Some(names))
    }
    /// The variants an enum item spells (#1200). An enum body holds no macro call, and an
    /// attribute macro cannot apply to a variant, so its text lists every variant there is;
    /// only `cfg` and `cfg_attr` can leave one out. A variant is certain only when every
    /// attribute on it is a known inert one (`inert_variant_attribute`), so no spelling of
    /// a conditional (`#[cfg /* .. */ (..)]`, `#[r#cfg(..)]`) proves it. A variant written
    /// twice, or in a body the parser could not read, is not certain.
    fn record_variants(
        &mut self,
        doc: &DocumentInput,
        document: usize,
        item: Node<'_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let Some(body) = item.child_by_field_name("body") else {
            return Ok(());
        };
        let readable = !body.has_error();
        let mut variants = BTreeMap::new();
        let mut conditional = false;
        let mut cursor = body.walk();
        for child in body.named_children(&mut cursor) {
            self.step(check)?;
            match child.kind() {
                // Any attribute but a known inert one could be a `cfg`, however spelled.
                "attribute_item" => conditional |= !inert_variant_attribute(doc, child),
                "enum_variant" => {
                    if let Some(name) = child.child_by_field_name("name") {
                        let braced = child
                            .child_by_field_name("body")
                            .is_some_and(|b| b.kind() == "field_declaration_list");
                        variants
                            .entry(text(doc, name).to_owned())
                            .and_modify(|v: &mut Variant| v.certain = false)
                            .or_insert(Variant {
                                certain: readable && !conditional,
                                braced,
                            });
                    }
                    conditional = false;
                }
                _ => {}
            }
        }
        self.variants
            .insert((document, item.start_byte(), item.end_byte()), variants);
        Ok(())
    }
    /// `Enum::Variant`: the names of the variant, which lives in the enum's own declaration,
    /// when `enumeration` is the one proved candidate before `rest`, an enum, and `rest` is
    /// one variant its body certainly spells (#1200). A variant is found before any
    /// associated item of the same name, so no `impl` or trait can take its place. A braced
    /// variant (`V { x: u8 }`) has no value constructor: it binds only the type namespace,
    /// as a braced struct does, so a value path `Enum::V` can name an associated item `V`
    /// and is not proved here. Anything
    /// else (an inherent or trait item, a type alias, a conditional variant, a path that
    /// goes on past the variant) is an unknown set.
    fn variant(
        &mut self,
        enumeration: Option<&Candidate>,
        rest: &[String],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        self.step(check)?;
        let unknown = Names {
            unknown_set: true,
            ..Names::default()
        };
        let (Some(enumeration), [name]) = (enumeration, rest) else {
            return Ok(unknown);
        };
        let declaration = &enumeration.declaration;
        let Some(&Variant {
            certain: true,
            braced,
        }) = self
            .variants
            .get(&(declaration.document, declaration.start, declaration.end))
            .and_then(|variants| variants.get(name))
        else {
            return Ok(unknown);
        };
        let mut names = Names::default();
        let namespaces: &[u8] = if braced { &[TYPE] } else { &[TYPE, VALUE] };
        for &namespace in namespaces {
            names
                .names
                .entry((namespace, name.to_owned()))
                .or_default()
                .candidates
                .insert(declaration.clone(), enumeration.clone());
        }
        Ok(names)
    }
    /// A module prefix alone, useful as an expansion target, not a synthetic declaration.
    fn module_name(&self, module: usize, name: &str, depth: usize) -> Names {
        let mut names = Names::default();
        let document = self.scopes[module].document;
        let candidate = Candidate {
            declaration: self.file_declarations[&document].clone(),
            module: Some(module),
            follow: depth,
            proof: BTreeSet::from([document]),
        };
        names
            .names
            .entry((TYPE, name.to_owned()))
            .or_default()
            .candidates
            .insert(candidate.declaration.clone(), candidate);
        names
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
        // The glob's path resolves in its defining crate; the caller's context still filters
        // the names the target exports.
        let target = self.path(
            scope,
            &import.path,
            true,
            depth,
            route,
            ResolveContext {
                foreign: false,
                ..context
            },
            check,
        )?;
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
                false,
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
        let key = (module, depth, context.waive_external, context.foreign);
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
            if self.scopes[module].explicit[&key]
                .iter()
                .any(|b| b.visible(context))
            {
                spellings.insert(key.1);
            }
        }
        let imports = self.scope_imports(module, check)?;
        let mut result = Names {
            unknown_set: self.scopes[module].opaque,
            ..Names::default()
        };
        for import in imports.iter().filter(|i| i.visible(context)) {
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
            let resolved = self.binding(module, &name, true, None, depth, route, context, check)?;
            result.unknown_set |= resolved.unknown_set;
            for (key, bucket) in resolved.names {
                result.names.insert(key, bucket);
            }
        }
        Ok(result)
    }
    /// The dependency the first segment of `path` names from `scope`, when no nearer scope
    /// binds it. A glob whose exports are unknown could bind it; for external attribution
    /// such a glob over a declared dependency is waived. `strict` waives nothing: a known
    /// attribute macro's proof must rule out every glob, since a dependency's glob can
    /// export a module of the same name that shadows the extern prelude.
    #[allow(clippy::too_many_arguments)] // The lookup's scope, path, waiver and captured context.
    fn external_target(
        &mut self,
        scope: usize,
        path: &[String],
        packages: &external::RustPackages,
        docs: &[&DocumentInput],
        excluding: Option<(usize, usize)>,
        strict: bool,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<external::ExternalPackage>> {
        self.step(check)?;
        let Some(first) = path.first() else {
            return Ok(None);
        };
        if matches!(first.as_str(), "crate" | "self" | "super") || self.root(scope).is_none() {
            return Ok(None);
        }
        // `excluding` is the import whose path this is: a `use` path, which edition 2015
        // starts at the crate root (#1166). The walk below reads the scopes of its module,
        // which in the root module include the root's own.
        if excluding.is_some()
            && self.root(scope) != Some(self.scopes[scope].module)
            && self.root_reading(scope, first, check)? == RootReading::Other
        {
            return Ok(None);
        }
        if self.prelude_blocked(scope, first, packages, docs, excluding, strict, check)? {
            return Ok(None);
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
    /// Whether a scope from `scope` up to its module binds `first`, or could, before the
    /// extern prelude, as `external_target` reads it. A walk with no import excluded reads
    /// the same scopes from each scope it passes, so its verdict is remembered for every one
    /// of them (#1210) and a later walk stops at the first it reaches. The verdict is
    /// recorded only once the walk that found it was charged; a hit is charged its step.
    #[allow(clippy::too_many_arguments)] // As `external_target`.
    fn prelude_blocked(
        &mut self,
        scope: usize,
        first: &str,
        packages: &external::RustPackages,
        docs: &[&DocumentInput],
        excluding: Option<(usize, usize)>,
        strict: bool,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        let remember = excluding.is_none() && self.memos();
        let mut visited = Vec::new();
        let mut current = Some(scope);
        let blocked = loop {
            let Some(at) = current else {
                break false;
            };
            self.step(check)?;
            if remember {
                if let Some(&blocked) = self.prelude_memo.get(&(at, first.to_owned(), strict)) {
                    break blocked;
                }
                visited.push(at);
            }
            let excluded = excluding.filter(|_| at == scope);
            if self.blocks_prelude(at, first, packages, docs, excluded, strict, check)? {
                break true;
            }
            current = if at == self.scopes[at].module {
                None
            } else {
                self.scopes[at].parent
            };
        };
        for at in visited {
            self.prelude_memo
                .insert((at, first.to_owned(), strict), blocked);
        }
        Ok(blocked)
    }
    /// Whether scope `at` itself binds `first`, or could: by an item, a named import other
    /// than `excluded`, or a glob that offers it or, unless waived, exports unknown names.
    #[allow(clippy::too_many_arguments)] // As `external_target`.
    fn blocks_prelude(
        &mut self,
        at: usize,
        first: &str,
        packages: &external::RustPackages,
        docs: &[&DocumentInput],
        excluded: Option<(usize, usize)>,
        strict: bool,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        if self.scopes[at].opaque
            || self.scopes[at]
                .explicit
                .contains_key(&(TYPE, first.to_owned()))
        {
            return Ok(true);
        }
        let imports = self.scope_imports(at, check)?;
        for import in imports {
            self.step(check)?;
            if excluded == Some((import.start, import.end)) {
                continue;
            }
            if !import.star && import.binding_name() == Some(first) {
                return Ok(true);
            }
            if import.star {
                let expanded = self.expand(
                    at,
                    &import,
                    1,
                    &mut BTreeSet::new(),
                    ResolveContext {
                        packages,
                        docs,
                        waive_external: false,
                        foreign: false,
                    },
                    Some(first),
                    check,
                )?;
                if expanded
                    .names
                    .get(&(TYPE, first.to_owned()))
                    .is_some_and(|b| b.unknown || !b.candidates.is_empty())
                {
                    return Ok(true);
                }
                if expanded.unknown_set {
                    if strict {
                        return Ok(true);
                    }
                    let document = self.scopes[at].document;
                    let source_set = &docs[document].key.source_set_id;
                    let root = self.root(at).expect("proved root");
                    let root_path = &docs[self.scopes[root].document].key.path;
                    let declared_external = !import.uncertain
                        && import.path.first().is_some_and(|name| {
                            !self.scopes[at].explicit.contains_key(&(TYPE, name.clone()))
                                && packages
                                    .resolve(
                                        source_set,
                                        name,
                                        packages.root_allowed(source_set, root_path),
                                    )
                                    .is_some()
                        });
                    if !declared_external {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
    /// Removes from a private glob's `names` those its scope never spells, and says whether it
    /// removed any (#986). A use is an identifier anywhere in the scope outside the glob,
    /// macro arguments and nested items included, so a shadowed or unrelated identifier of the
    /// same spelling still counts. Syntax the walk cannot read proves no name unused.
    fn drop_unused(
        &mut self,
        doc: &DocumentInput,
        tree: &Tree,
        scope: usize,
        import: &Import,
        names: &mut Names,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        self.step(check)?;
        if names.names.is_empty() {
            return Ok(false);
        }
        let (start, end) = (self.scopes[scope].start, self.scopes[scope].end);
        let Some(root) = tree.root_node().descendant_for_byte_range(start, end) else {
            return Ok(false);
        };
        if root.has_error() {
            return Ok(false);
        }
        self.charge(names.names.len(), check)?;
        // `r#name` and `name` are one identifier, whichever side spells the prefix.
        let plain = |name: &str| name.strip_prefix("r#").unwrap_or(name).to_owned();
        let mut unseen: BTreeSet<String> =
            names.names.keys().map(|(_, name)| plain(name)).collect();
        let mut stack = vec![(root, 0)];
        let mut visits = 0;
        while let Some((node, depth)) = stack.pop() {
            if unseen.is_empty() {
                return Ok(false);
            }
            check()?;
            visits += 1;
            if visits > VISITS || depth > DEPTH {
                return Ok(false);
            }
            if node.start_byte() == import.start && node.end_byte() == import.end {
                continue;
            }
            if matches!(
                node.kind(),
                "identifier" | "type_identifier" | "shorthand_field_identifier"
            ) {
                unseen.remove(&plain(text(doc, node)));
                continue;
            }
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                stack.push((child, depth + 1));
            }
        }
        names
            .names
            .retain(|(_, name), _| !unseen.contains(&plain(name)));
        Ok(!unseen.is_empty())
    }
    #[allow(clippy::too_many_arguments)] // Captured inputs and the site's own tree stay explicit.
    pub(super) fn resolve(
        &mut self,
        document: usize,
        site: &RawSite,
        packages: &external::RustPackages,
        external_packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
        docs: &[&DocumentInput],
        tree: &Tree,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        self.step(check)?;
        let Some(scope) = self.scope_at(document, site.start, site.end, check)? else {
            return Ok(SiteResolution::unknown("rust_scope_unproved"));
        };
        if self.root(scope).is_none() {
            return Ok(SiteResolution::unknown("rust_module_ownership_unproved"));
        }
        let import = self.find_import_at(scope, site, check)?;
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
                false,
                check,
            )?
        {
            return self.record_external(package, external_packages, check);
        }
        let context = ResolveContext {
            packages,
            docs,
            waive_external: true,
            foreign: false,
        };
        let mut route = BTreeSet::new();
        let mut shadowed = false;
        let mut unused = false;
        let names = if let Some(import) = &import {
            if import.uncertain {
                return Ok(SiteResolution::unknown("rust_conditional_export"));
            }
            if import.star {
                self.star_names(
                    document,
                    scope,
                    import,
                    tree,
                    &mut route,
                    context,
                    &mut shadowed,
                    &mut unused,
                    check,
                )?
            } else {
                self.path(scope, &import.path, true, 1, &mut route, context, check)?
            }
        } else {
            match parts(&site.spelling) {
                Some(path) => self.site_path(scope, path, &mut route, context, check)?,
                None => return Ok(SiteResolution::unknown("rust_path_unsupported")),
            }
        };
        self.publish_names(site, &import, names, shadowed, unused, check)
    }
    /// The names a site's own path reaches from `scope` (#1210). A block that cannot bind
    /// the first segment answers nothing there, so the lexical walk would climb past it: the
    /// answer is the one from the first scope that could bind it, the same for every site
    /// whose scope climbs there, and remembered by that scope and the path. It is recorded
    /// only once the walk that found it was charged; each read, the first included, is
    /// charged a step for each block climbed and its copy, as `copy_names` charges one.
    fn site_path(
        &mut self,
        scope: usize,
        path: Vec<String>,
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        if !self.memos() {
            return self.path(scope, &path, false, 1, route, context, check);
        }
        let mut start = scope;
        if let Some(first) = path.first() {
            while let Some(parent) = self.binds_nothing(start, first, route) {
                self.step(check)?;
                start = parent;
            }
        }
        let key = (start, path);
        let names = match self.path_memo.get(&key).cloned() {
            Some(names) => names,
            None => {
                let names = self.path(start, &key.1, false, 1, route, context, check)?;
                let names = std::sync::Arc::new(names);
                self.path_memo.insert(key, names.clone());
                names
            }
        };
        self.step(check)?;
        self.copy_names(&names, None, check)
    }
    /// Whether the prelude and path memos are read: always.
    #[cfg(not(test))]
    fn memos(&self) -> bool {
        true
    }
    /// Whether the prelude and path memos are read: tests compare reads without them.
    #[cfg(test)]
    fn memos(&self) -> bool {
        self.memos
    }
    /// The parent of block `scope` when the block cannot bind `name`: it declares nothing of
    /// that name in either namespace and imports nothing of it, has no glob, is read whole,
    /// and is not on the route. `binding` would find nothing there, and no export
    /// computation traces what it consults.
    fn binds_nothing(&self, scope: usize, name: &str, route: &Route) -> Option<usize> {
        let at = &self.scopes[scope];
        let key = |namespace| (namespace, name.to_owned());
        if scope == at.module
            || at.opaque
            || at.cut
            || !self.export_traces.is_empty()
            || at.explicit.contains_key(&key(TYPE))
            || at.explicit.contains_key(&key(VALUE))
            || at.imports.named.contains_key(name)
            || !at.imports.stars.is_empty()
            || route.contains(&(scope, name.to_owned()))
        {
            return None;
        }
        at.parent
    }
    /// The import the site is, when its range is one of the scope's imports.
    fn find_import_at(
        &mut self,
        scope: usize,
        site: &RawSite,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Import>> {
        let imports = self.scope_imports(scope, check)?;
        let mut import = None;
        for candidate in imports {
            self.step(check)?;
            if candidate.start == site.start && candidate.end == site.end {
                import = Some(candidate);
                break;
            }
        }
        Ok(import)
    }
    /// A site that reaches an external package: the package joins the read's packages, or its
    /// evidence joins the entry already there.
    fn record_external(
        &mut self,
        package: external::ExternalPackage,
        external_packages: &mut BTreeMap<external::ExternalIdentity, external::ExternalPackage>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
        let identity = package.identity.clone();
        self.charge(package.evidence.len(), check)?;
        if let Some(existing) = external_packages.get_mut(&identity) {
            existing.evidence.extend(package.evidence);
            existing.evidence.sort();
            existing.evidence.dedup();
        } else {
            external_packages.insert(identity.clone(), package);
        }
        Ok(SiteResolution {
            targets: BTreeSet::from([Target::External(identity)]),
            ..SiteResolution::default()
        })
    }
    /// The names a glob import brings into its scope: what it expands to, less the names a
    /// private glob's scope never spells and the names the scope binds explicitly, each
    /// marked by what the scope's other globs also offer. `route` is the site's, shared
    /// with every expansion and path walk; the two flags say why a name was dropped.
    #[allow(clippy::too_many_arguments)] // The site's tree, route and flags stay the caller's.
    fn star_names(
        &mut self,
        document: usize,
        scope: usize,
        import: &Import,
        tree: &Tree,
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        shadowed: &mut bool,
        unused: &mut bool,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        let ResolveContext { docs, .. } = context;
        let mut own = self.expand(scope, import, 1, route, context, None, check)?;
        // A re-exporting glob passes every name on; a private one references only
        // the names its scope spells (#986).
        if !import.public {
            *unused = self.drop_unused(docs[document], tree, scope, import, &mut own, check)?;
        }
        let others = self.scope_imports(scope, check)?;
        let competing = self.competing_star_names(scope, import, &others, route, context, check)?;
        self.charge(own.names.len(), check)?;
        let keys: Vec<_> = own.names.keys().cloned().collect();
        for key in keys {
            self.step(check)?;
            let explicit = self.name_is_explicit(scope, &key, &others, route, context, check)?;
            if explicit {
                own.names.remove(&key);
                *shadowed = true;
                continue;
            }
            let bucket = own.names.get_mut(&key).expect("existing name");
            bucket.unknown |= competing.unknown_set || self.scopes[scope].opaque;
            if let Some(other) = competing.names.get(&key) {
                self.merge_bucket(bucket, other.clone(), check)?;
            }
        }
        Ok(own)
    }
    /// What the scope's other globs offer, merged. A glob that reaches an external package
    /// offers nothing here.
    fn competing_star_names(
        &mut self,
        scope: usize,
        import: &Import,
        others: &[Import],
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Names> {
        let ResolveContext { packages, docs, .. } = context;
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
                    false,
                    check,
                )?
                .is_some()
            {
                continue;
            }
            let expanded = self.expand(scope, other, 1, route, context, None, check)?;
            competing.unknown_set |= expanded.unknown_set;
            for (key, bucket) in expanded.names {
                self.merge_bucket(competing.names.entry(key).or_default(), bucket, check)?;
            }
        }
        Ok(competing)
    }
    /// Whether the scope binds `key` itself, by a declaration or by a named import that
    /// resolves in the key's namespace or cannot be read.
    fn name_is_explicit(
        &mut self,
        scope: usize,
        key: &Name,
        others: &[Import],
        route: &mut Route,
        context: ResolveContext<'_, '_>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        let direct = self.scopes[scope].explicit.contains_key(key);
        let mut explicit_imports = Vec::new();
        for other in others {
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
                self.path(scope, &named.path, true, 1, route, context, check)?
            };
            explicit |= resolved.unknown_set
                || resolved
                    .names
                    .keys()
                    .any(|(namespace, _)| *namespace == key.0);
        }
        Ok(explicit)
    }
    /// The site's resolution from the names found: the targets and proof of each name in the
    /// site's namespace, and the residual when the set is unknown, a name is ambiguous or
    /// nothing is left.
    fn publish_names(
        &mut self,
        site: &RawSite,
        import: &Option<Import>,
        mut names: Names,
        shadowed: bool,
        unused: bool,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<SiteResolution> {
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
                } else if unused {
                    "rust_wildcard_unused"
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
/// One child of a scanned node, with what the attributes before it said about it. Data only:
/// each pass of `scan` destructures it into the names its statements use.
#[derive(Clone, Copy)]
struct Item<'a, 'tree> {
    doc: &'a DocumentInput,
    document: usize,
    scope: usize,
    child: Node<'tree>,
    item_uncertain: bool,
    item_custom: bool,
}
/// What the attributes read so far say about the next item.
#[derive(Default)]
struct Pending {
    uncertain: bool,
    custom_path: bool,
    /// The helper attributes that a derive written earlier on this item registers.
    helpers: BTreeSet<&'static str>,
    /// The known attribute macros on this item, in source order.
    known: Vec<KnownMacro>,
}
/// An attribute macro whose expansion binds no name the item's scope can see and changes
/// no path the item spells, once its path is proved to reach the macro's own package and
/// the item is one its expansion is read for (owner ruling 2026-11-17). `global` is a path
/// written with a leading `::`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum KnownMacro {
    /// `#[tokio::main]`, `#[tokio::test]`, with any arguments. tokio-macros re-emits the
    /// function alone (`entry.rs` `into_tokens`), its original body kept as the
    /// initializer of `let body = async { .. };`, where `body` is not yet in scope; the
    /// runtime is built in a sibling block whose `use` the body cannot see. It lifts inner
    /// attributes of the body onto the function, so the body must have none.
    Tokio { test: bool, global: bool },
    /// `#[async_trait]` (`bare`, through an import) or `#[async_trait::async_trait]`, with
    /// any arguments, on a trait or trait impl. async-trait re-emits the item alone, each
    /// async method's body moved into `Box::pin(async move { .. })` after bindings that
    /// change no path the body spells: `__ret` (not yet in scope in the body), mixed-site
    /// `__argN` (invisible to it) and `let x = x;` (the same parameter under the same
    /// name). The one exception is `__self`, which takes the place of `self` and is visible
    /// to the body: so the item must not spell `__self`.
    AsyncTrait { bare: bool, global: bool },
}
/// One variant an enum body spells (#1200).
#[derive(Clone, Copy)]
struct Variant {
    /// No attribute on it but known inert ones, and spelled once in a readable body: it
    /// certainly exists.
    certain: bool,
    /// A braced (record) variant, which binds the type namespace only.
    braced: bool,
}
/// An item under known attribute macros, with the bindings it made, which the failed proof
/// of any of them leaves uncertain, as under an attribute this catalog does not read.
struct MacroProof {
    scope: usize,
    macros: Vec<KnownMacro>,
    bindings: Vec<(Name, usize)>,
    failed: bool,
}
/// Where a statement macro's name must be proved to lead before its call is read as binding
/// nothing (#1210). Each source has its table of names below, an arm in `macro_source`, the
/// paths `statement_macro` accepts for it and an arm in `proves_statement_macro`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum MacroSource {
    /// `STANDARD_LIBRARY_MACROS`, bare or under `std`, `core` or `alloc`.
    StandardLibrary,
    /// A dependency's statement macro (#1223), from `PACKAGE_MACROS`: the package's crate
    /// name, which `proves_package` proves, bare through an import or under that name.
    Package(&'static str),
}
/// The source whose macro a statement macro of this name would be, if any.
fn macro_source(name: &str) -> Option<MacroSource> {
    if STANDARD_LIBRARY_MACROS.contains(&name) {
        return Some(MacroSource::StandardLibrary);
    }
    PACKAGE_MACROS
        .iter()
        .find(|(_, names)| names.contains(&name))
        .map(|&(package, _)| MacroSource::Package(package))
}
/// Dependencies' statement macros (#1223, owner ruling 2026-11-19), by package: each
/// expands to an expression, or to an early `return`, and binds no name.
const PACKAGE_MACROS: &[(&str, &[&str])] = &[
    ("serde_json", &["json"]),
    ("rusqlite", &["params", "named_params"]),
    ("anyhow", &["anyhow", "bail", "ensure"]),
    ("tracing", &["trace", "debug", "info", "warn", "error"]),
];
/// The standard library's statement macros (#1210, owner ruling 2026-11-18): each expands
/// to an expression and binds no name, except `thread_local!`, which declares exactly the
/// statics it spells. `include!` is not one: its expansion is another file's items.
const STANDARD_LIBRARY_MACROS: &[&str] = &[
    "format",
    "format_args",
    "print",
    "println",
    "eprint",
    "eprintln",
    "write",
    "writeln",
    "vec",
    "matches",
    "assert",
    "assert_eq",
    "assert_ne",
    "debug_assert",
    "debug_assert_eq",
    "debug_assert_ne",
    "panic",
    "unreachable",
    "todo",
    "unimplemented",
    "concat",
    "stringify",
    "include_str",
    "include_bytes",
    "env",
    "option_env",
    "line",
    "column",
    "file",
    "module_path",
    "cfg",
    "dbg",
    "thread_local",
];
/// The macro calls below a statement.
enum MacroCalls {
    None,
    /// Statement macros only.
    Statement,
    /// Another macro, which could bind any name.
    Other,
}
/// What across the checkout could give a statement macro's bare name another meaning
/// (#1210).
#[derive(Default)]
struct MacroShadows {
    /// Every name a `macro_rules!` in a file's text could define.
    rules: BTreeSet<String>,
    /// A parsed `#[macro_use]` or `#![macro_use]`.
    macro_use: bool,
    /// A parsed inner attribute naming the `decl_macro` feature.
    decl_macro: bool,
    /// A Rust file whose text was not read, or a file with a parser error that spells
    /// either attribute: it could define anything.
    unread: bool,
}
impl MacroShadows {
    /// Whether any of these could give a statement macro's bare name another meaning.
    fn shadow(&self, name: &str) -> bool {
        self.unread || self.macro_use || self.decl_macro || self.rules.contains(name)
    }
}
/// A statement macro call held until its proof (#1210).
struct StatementMacro {
    /// The scope the call stands in.
    scope: usize,
    /// The scope of the outermost expression statement holding the call, or `scope`.
    owner: usize,
    /// What the call's name must be proved to lead to.
    source: MacroSource,
    /// `[name]`, or the path written before it, as `[std, name]`.
    path: Vec<String>,
    /// Written with a leading `::`.
    global: bool,
    /// A bare name proved once: its proof reads nothing a failed proof changes.
    settled: bool,
    failed: bool,
}
/// The source, path and leading `::` of a statement macro call: a bare name of a source's
/// table, or a path that source accepts (for the standard library, the name under `std`,
/// `core` or `alloc`; for a package, under its crate name). A `thread_local!` must stand as a statement of a block or module,
/// where it declares its statics.
fn statement_macro(
    doc: &DocumentInput,
    call: Node<'_>,
) -> Option<(MacroSource, Vec<String>, bool)> {
    let spelled: String = text(doc, call.child_by_field_name("macro")?)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let global = spelled.starts_with("::");
    let path = parts(spelled.strip_prefix("::").unwrap_or(&spelled))?;
    let (name, prefix) = path.split_last()?;
    let source = macro_source(name)?;
    let accepted = match (source, prefix) {
        (_, []) => !global,
        (MacroSource::StandardLibrary, [first]) => {
            matches!(first.as_str(), "std" | "core" | "alloc")
        }
        (MacroSource::Package(package), [first]) => first == package,
        _ => false,
    };
    if !accepted {
        return None;
    }
    if name == "thread_local" {
        let holder = |node: Option<Node<'_>>| {
            node.is_some_and(|node| {
                matches!(node.kind(), "block" | "source_file" | "declaration_list")
            })
        };
        let parent = call.parent();
        let statement = holder(parent)
            || parent.is_some_and(|p| p.kind() == "expression_statement" && holder(p.parent()));
        if !statement {
            return None;
        }
    }
    Some((source, path, global))
}
/// Every name a `macro_rules!` definition in `text` could take: an identifier, `r#` removed,
/// after `macro_rules` and `!`, with any whitespace or comments between.
fn macro_rules_names(text: &str) -> BTreeSet<String> {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut names = BTreeSet::new();
    for (at, keyword) in text.match_indices("macro_rules") {
        if text[..at].chars().next_back().is_some_and(ident) {
            continue;
        }
        let Some(rest) = skip_trivia(&text[at + keyword.len()..]).strip_prefix('!') else {
            continue;
        };
        let rest = skip_trivia(rest);
        let rest = rest.strip_prefix("r#").unwrap_or(rest);
        let name: String = rest.chars().take_while(|&c| ident(c)).collect();
        if !name.is_empty() {
            names.insert(name);
        }
    }
    names
}
/// `text` after any leading whitespace and comments (block comments nest).
fn skip_trivia(mut text: &str) -> &str {
    loop {
        let trimmed = text.trim_start();
        if let Some(rest) = trimmed.strip_prefix("//") {
            text = rest.find('\n').map_or("", |end| &rest[end..]);
        } else if trimmed.starts_with("/*") {
            let mut depth = 0usize;
            let mut end = trimmed.len();
            let bytes = trimmed.as_bytes();
            let mut at = 0;
            while at + 1 < bytes.len() {
                match &bytes[at..at + 2] {
                    b"/*" => {
                        depth += 1;
                        at += 2;
                    }
                    b"*/" => {
                        depth -= 1;
                        at += 2;
                        if depth == 0 {
                            end = at;
                            break;
                        }
                    }
                    _ => at += 1,
                }
            }
            text = &trimmed[end..];
        } else {
            return trimmed;
        }
    }
}
/// The known attribute macro an outer attribute's path names, whatever its arguments.
fn known_macro(name: &str) -> Option<KnownMacro> {
    let (global, path) = match name.strip_prefix("::") {
        Some(path) => (true, path),
        None => (false, name),
    };
    match path {
        "tokio::main" => Some(KnownMacro::Tokio {
            test: false,
            global,
        }),
        "tokio::test" => Some(KnownMacro::Tokio { test: true, global }),
        // `::async_trait` alone names the crate, not its macro.
        "async_trait" if !global => Some(KnownMacro::AsyncTrait { bare: true, global }),
        "async_trait::async_trait" => Some(KnownMacro::AsyncTrait {
            bare: false,
            global,
        }),
        _ => None,
    }
}
/// The attributes a variant can carry that never make it conditional (#1200): built-in
/// inert attributes and the variant helpers of serde, thiserror and clap. The path is read
/// from the parsed attribute, one identifier with any `r#` removed, so comments and raw
/// spellings change nothing; any other path, `cfg` and `cfg_attr` included, is refused.
fn inert_variant_attribute(doc: &DocumentInput, item: Node<'_>) -> bool {
    const INERT: &[&str] = &[
        "doc",
        "allow",
        "warn",
        "deny",
        "forbid",
        "expect",
        "deprecated",
        "default",
        "serde",
        "error",
        "command",
        "value",
    ];
    let mut cursor = item.walk();
    let Some(attribute) = item
        .named_children(&mut cursor)
        .find(|n| n.kind() == "attribute")
    else {
        return false;
    };
    let mut cursor = attribute.walk();
    let path = attribute
        .named_children(&mut cursor)
        .find(|n| !matches!(n.kind(), "line_comment" | "block_comment"));
    path.is_some_and(|path| {
        let name = text(doc, path);
        path.kind() == "identifier" && INERT.contains(&name.strip_prefix("r#").unwrap_or(name))
    })
}
/// Whether `text` spells the identifier `name`, not as part of a longer identifier.
fn spells_identifier(text: &str, name: &str) -> bool {
    let ident = |c: char| c.is_alphanumeric() || c == '_';
    text.match_indices(name).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(ident)
            && !text[at + name.len()..].chars().next().is_some_and(ident)
    })
}
/// Built-in attributes that bind no name, in either the outer or the inner form. A macro of
/// one of these names cannot be used as an attribute: the name is ambiguous with the
/// built-in. `test` is read before this list; `cfg`, `cfg_attr`, `path` and `derive` have
/// meanings of their own.
const INERT_ATTRIBUTES: &[&str] = &[
    "allow",
    "warn",
    "deny",
    "forbid",
    "expect",
    "doc",
    "inline",
    "repr",
    "must_use",
    "cold",
    "no_mangle",
    "ignore",
    "should_panic",
    "track_caller",
    "non_exhaustive",
    "deprecated",
    "link",
    "recursion_limit",
    "no_main",
    "global_allocator",
];
/// The path of an attribute with its whitespace removed: `#[a::b(..)]` and `#![a::b = ..]`
/// give `a::b`.
fn attribute_name(attribute: &str) -> &str {
    let rest = attribute
        .strip_prefix("#![")
        .or_else(|| attribute.strip_prefix("#["))
        .unwrap_or(attribute);
    let end = rest.find(['(', '[', '{', '=', ']']).unwrap_or(rest.len());
    &rest[..end]
}
/// The helper attributes that a derive of this name registers: serde's, clap's (4.x) and
/// thiserror's.
fn derive_helpers(derive: &str) -> &'static [&'static str] {
    match derive {
        "Serialize" | "Deserialize" => &["serde"],
        "Parser" | "Args" | "Subcommand" => &["clap", "command", "arg", "group"],
        "ValueEnum" => &["clap", "value"],
        "Error" => &["error"],
        _ => &[],
    }
}
/// The namespaces an item's name is bound in; none for a node that binds no name.
fn item_namespaces(child: Node<'_>) -> &'static [u8] {
    match child.kind() {
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
/// The module path of a restricted visibility other than `pub(crate)` (#1209): `pub(self)`
/// and `pub(super)` read as `pub(in self)` and `pub(in super)`. Only a path that starts with
/// `crate`, `self` or `super` is read, as edition 2018 requires; anything else, a comment
/// inside the parentheses included, is `None`.
fn restriction_path(visibility: &str) -> Option<Vec<String>> {
    let inner = visibility
        .strip_prefix("pub")?
        .trim_start()
        .strip_prefix('(')?
        .strip_suffix(')')?
        .trim();
    let path = match inner.strip_prefix("in") {
        Some(rest) if rest.starts_with(char::is_whitespace) => rest,
        _ => inner,
    };
    parts(path).filter(|path| matches!(path[0].as_str(), "crate" | "self" | "super"))
}
/// Whether `node` is `pub(crate)`.
fn crate_visibility(doc: &DocumentInput, node: Node<'_>) -> bool {
    visibility(doc, node).is_some_and(|v| {
        v.chars().filter(|c| !c.is_whitespace()).collect::<String>() == "pub(crate)"
    })
}
impl Import {
    fn visible(&self, context: ResolveContext<'_, '_>) -> bool {
        self.public && !(self.crate_only && context.foreign)
    }
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
    (public, crate_only): (bool, bool),
    uncertain: bool,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<(Vec<Import>, bool)> {
    let mut pending = vec![(root, Vec::<String>::new(), 0usize)];
    let mut result = Vec::new();
    let mut cut = false;
    while let Some((node, prefix, depth)) = pending.pop() {
        owner_step(work, check)?;
        // Each nesting level clones the prefix, so a deep list is bounded here too.
        if depth > DEPTH {
            cut = true;
            continue;
        }
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
                    pending.push((list, combine(path), depth + 1));
                }
            }
            "use_list" => {
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    owner_step(work, check)?;
                    pending.push((child, prefix.clone(), depth + 1));
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
                    crate_only,
                    uncertain,
                });
            }
        }
    }
    Ok((result, cut))
}

/// The words of a source's text, whatever its syntax, and whether a `mod` has no name left to
/// read. A parser error can break the keyword too, so any word could have been a module.
fn possible_module_names(text: &str) -> (BTreeSet<&str>, bool) {
    let identifier = |c: char| c.is_alphanumeric() || c == '_';
    let words: BTreeSet<_> = text
        .split(|c| !identifier(c))
        .filter(|word| !word.is_empty())
        .collect();
    let unnamed = text.match_indices("mod").any(|(at, _)| {
        let tail = text[at + 3..].trim_start();
        !text[..at].ends_with(identifier)
            && !text[at + 3..].starts_with(identifier)
            && !tail.starts_with(identifier)
            && !tail.starts_with("r#")
    });
    (words, unnamed)
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
                for import in imports(doc, argument, (false, false), false, work, check)?.0 {
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

/// What the attributes on and around one site say about it (#988).
#[derive(Default)]
pub(super) struct SiteMarks {
    /// An attribute marks test code under Decision 0011.
    pub(super) test: bool,
    /// The features of the `cfg(feature = "..")` gates on the site, sorted and distinct.
    pub(super) features: Vec<String>,
}

/// A Cargo feature name a rules file may name and a gate may carry.
pub(crate) fn feature_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '.'))
}

/// One token of a `cfg` attribute: a word, a plain string literal's exact value, or a
/// punctuation character.
#[derive(Clone, Copy, PartialEq)]
enum GateToken<'a> {
    Word(&'a str),
    Text(&'a str),
    Mark(char),
}

/// The tokens of an attribute, with whitespace between tokens dropped and a string literal's
/// value kept exactly. `None` for what this reader does not interpret: a comment, an escape,
/// an unterminated or raw literal, or more tokens than any gate it accepts has.
fn gate_tokens(attribute: &str) -> Option<Vec<GateToken<'_>>> {
    const MOST: usize = 20;
    let mut tokens = Vec::new();
    let mut rest = attribute;
    while let Some(first) = rest.chars().next() {
        if tokens.len() == MOST {
            return None;
        }
        if first.is_whitespace() {
            rest = &rest[first.len_utf8()..];
        } else if first == '"' {
            let end = rest[1..].find('"')?;
            let value = &rest[1..1 + end];
            if value.contains('\\') {
                return None;
            }
            tokens.push(GateToken::Text(value));
            rest = &rest[end + 2..];
        } else if first.is_ascii_alphabetic() || first == '_' {
            let end = rest
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            // `r"…"`, `r#"…"#`, `b"…"` and `c"…"` are literals this reader does not interpret.
            if rest[end..].starts_with(['"', '#']) {
                return None;
            }
            tokens.push(GateToken::Word(&rest[..end]));
            rest = &rest[end..];
        } else if first == '/' {
            return None;
        } else {
            tokens.push(GateToken::Mark(first));
            rest = &rest[first.len_utf8()..];
        }
    }
    Some(tokens)
}

/// The feature of a gate whose whole condition is `feature = "name"` or `any(test, feature =
/// "name")`, the two operands in either order, as an outer or inner `cfg` attribute. The
/// name is the literal's exact value; only whitespace between tokens is ignored. Any other
/// condition names none: `not(..)`, `all(..)`, an `any` with other or more operands, and
/// `cfg_attr`.
fn feature_gate(attribute: &str) -> Option<String> {
    use GateToken::{Mark, Text, Word};
    if !attribute.contains("feature") {
        return None;
    }
    let tokens = gate_tokens(attribute)?;
    let tokens = tokens.strip_prefix(&[Mark('#')])?;
    let tokens = tokens.strip_prefix(&[Mark('!')]).unwrap_or(tokens);
    let condition = tokens
        .strip_prefix(&[Mark('['), Word("cfg"), Mark('(')])?
        .strip_suffix(&[Mark(')'), Mark(']')])?;
    let name = match condition {
        [Word("feature"), Mark('='), Text(name)]
        | [
            Word("any"),
            Mark('('),
            Word("test"),
            Mark(','),
            Word("feature"),
            Mark('='),
            Text(name),
            Mark(')'),
        ]
        | [
            Word("any"),
            Mark('('),
            Word("feature"),
            Mark('='),
            Text(name),
            Mark(','),
            Word("test"),
            Mark(')'),
        ] => *name,
        _ => return None,
    };
    feature_name(name).then(|| name.to_owned())
}

/// The marks of the syntax at `start..end` and of everything enclosing it: whether an
/// attribute marks test code under Decision 0011, and otherwise the features that gate it
/// (#988). The pinned grammar attaches an attribute in one of three shapes, and every
/// enclosing node is checked for all of them:
///
/// - an outer attribute is a preceding sibling of what it applies to: an item or statement,
///   a field, an enum variant, a parameter, a generic parameter, or an element of an argument
///   list, array or tuple. A tuple-struct field's visibility lies between the two;
/// - a match arm and a struct-expression field own their outer attributes as leading children;
/// - an inner attribute leads the file, block or module body it applies to.
pub(super) fn site_marks(
    doc: &DocumentInput,
    tree: &Tree,
    start: usize,
    end: usize,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<SiteMarks> {
    let marks = |node: Node<'_>| crate::native_evidence::rust_test_attribute(node, &doc.file);
    let comment = |node: Node<'_>| matches!(node.kind(), "line_comment" | "block_comment");
    let test = || {
        Ok(SiteMarks {
            test: true,
            features: Vec::new(),
        })
    };
    let mut features = BTreeSet::new();
    let mut current = tree.root_node().descendant_for_byte_range(start, end);
    while let Some(node) = current {
        owner_step(work, check)?;
        // Any token but these, a separating comma included, ends the node's own attributes.
        let mut previous = node.prev_sibling();
        while let Some(sibling) = previous {
            owner_step(work, check)?;
            match sibling.kind() {
                "attribute_item" if marks(sibling) => return test(),
                "attribute_item" => features.extend(feature_gate(text(doc, sibling))),
                "inner_attribute_item" | "visibility_modifier" => {}
                _ if comment(sibling) => {}
                _ => break,
            }
            previous = sibling.prev_sibling();
        }
        let owns = matches!(
            node.kind(),
            "match_arm" | "field_initializer" | "shorthand_field_initializer"
        );
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            owner_step(work, check)?;
            match child.kind() {
                "inner_attribute_item" if marks(child) => return test(),
                "attribute_item" if owns && marks(child) => return test(),
                "inner_attribute_item" => features.extend(feature_gate(text(doc, child))),
                "attribute_item" if owns => features.extend(feature_gate(text(doc, child))),
                "attribute_item" => {}
                _ if comment(child) => {}
                _ => break,
            }
        }
        current = node.parent();
    }
    Ok(SiteMarks {
        test: false,
        features: features.into_iter().collect(),
    })
}

/// The path of module `name` in module directory `directory` (`""` is the checkout's).
fn child_path(directory: &str, name: &str) -> String {
    if directory.is_empty() {
        name.to_owned()
    } else {
        format!("{directory}/{name}")
    }
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
            imports: Imports::default(),
            opaque: false,
            cut: false,
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
                crate_only: false,
                restricted: None,
                uncertain: false,
                own_crate: false,
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
            crate_only: false,
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
            prelude_memo: BTreeMap::new(),
            path_memo: BTreeMap::new(),
            memos: true,
            files: BTreeMap::new(),
            file_declarations: BTreeMap::new(),
            ambiguous: BTreeSet::new(),
            externs: BTreeMap::new(),
            uniform: BTreeSet::new(),
            proofs: Vec::new(),
            variants: BTreeMap::new(),
            shadows: MacroShadows::default(),
            statement_macros: Vec::new(),
            statement_owner: None,
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
            foreign: false,
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
            foreign: false,
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
                foreign: false,
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
            foreign: false,
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
            foreign: false,
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

/// #1210: the prelude and path memos change what a read costs, never what it answers.
#[cfg(test)]
mod memo_tests {
    use super::*;
    use crate::references::{Snapshot, parallel_tests::snapshot_of};
    use sha2::{Digest, Sha256};

    const MANIFEST: &str = "[package]\nname = 'fixture'\nversion = '0.1.0'\nedition = '2021'\n\
        [dependencies]\nserde = '1'\ntokio = { version = '1', features = ['full'] }\n";

    /// A catalog of a snapshot's documents, its packages, and the documents with trees.
    struct Read<'a> {
        docs: Vec<&'a DocumentInput>,
        trees: Vec<Tree>,
        packages: external::RustPackages,
        catalog: Catalog,
    }

    /// `files` under `MANIFEST`, its documents in key order.
    fn snapshot(files: &[(&str, &str)]) -> Snapshot {
        let mut snapshot = snapshot_of(files);
        snapshot.documents.sort_by(|a, b| a.key.cmp(&b.key));
        snapshot
            .manifests
            .push(super::super::external::ManifestInput {
                source_set_id: snapshot.documents[0].key.source_set_id.clone(),
                native_revision_id: "native".into(),
                dependency_hash: "dependencies".into(),
                path: "Cargo.toml".into(),
                content_hash: Some(hex::encode(Sha256::digest(MANIFEST.as_bytes()))),
                bytes: Some(MANIFEST.as_bytes().to_vec()),
            });
        snapshot
    }

    fn read(snapshot: &Snapshot) -> Read<'_> {
        let mut check = || Ok(());
        let docs: Vec<&DocumentInput> = snapshot.documents.iter().collect();
        let trees: Vec<Tree> = docs
            .iter()
            .map(|doc| super::super::parse(doc, &mut check).unwrap())
            .collect();
        let documents = docs
            .iter()
            .enumerate()
            .map(|(index, doc)| (doc.key.clone(), index))
            .collect();
        let mut work = Work::new(work::reference_limit(0, 0));
        let packages =
            external::RustPackages::new(&snapshot.manifests, &mut work, &mut check).unwrap();
        let workspace = super::super::cargo_workspace::Workspace::new(
            &snapshot.manifests,
            &mut work,
            &mut check,
        )
        .unwrap();
        let catalog = Catalog::new(
            &docs, &trees, &documents, &workspace, &packages, work, &mut check,
        )
        .unwrap();
        Read {
            docs,
            trees,
            packages,
            catalog,
        }
    }

    /// Every site of the snapshot, resolved in document and site order on one catalog, with
    /// the memos or without them. Each answer is rendered with its site; the work is the
    /// resolution's alone.
    fn resolve_all(snapshot: &Snapshot, remember: bool) -> (Vec<String>, usize) {
        let Read {
            docs,
            trees,
            packages,
            mut catalog,
        } = read(snapshot);
        catalog.memos = remember;
        let mut check = || Ok(());
        let before = catalog.work.used;
        let mut external_packages = BTreeMap::new();
        let mut answers = Vec::new();
        for (document, doc) in docs.iter().enumerate() {
            let mut work = Work::new(work::reference_limit(0, 0));
            for site in extract(doc, &trees[document], &mut work, &mut check).unwrap() {
                let resolved = catalog
                    .resolve(
                        document,
                        &site,
                        &packages,
                        &mut external_packages,
                        &docs,
                        &trees[document],
                        &mut check,
                    )
                    .unwrap();
                let targets: Vec<String> = resolved
                    .targets
                    .iter()
                    .map(|target| match target {
                        Target::Native(index) => format!("native {index}"),
                        Target::External(identity) => format!("{identity:?}"),
                    })
                    .collect();
                answers.push(format!(
                    "{} {}..{} {:?}: {targets:?} proof {:?} residual {:?}",
                    doc.key.path,
                    site.start,
                    site.end,
                    site.spelling,
                    resolved.proof,
                    resolved.residual
                ));
            }
        }
        answers.push(format!(
            "{:?}",
            external_packages.keys().collect::<Vec<_>>()
        ));
        (answers, catalog.work.used - before)
    }

    /// Globs (a crate glob, `use super::*` in nested test modules, a dependency's glob),
    /// an opaque block and module, generic items, and blocks that shadow a glob's name by an
    /// item, a named import and a glob of their own.
    const FILES: &[(&str, &str)] = &[
        (
            "src/lib.rs",
            "mod store;\nmod other;\nuse store::*;\npub use other::Thing;\n\
             fn plain() { run(); run(); store::run(); crate::store::run(); Thing::new(); \
               serde::de::value(); tokio::spawn(); missing::path(); }\n\
             fn shadow() {\n\
               fn run() {}\n\
               run(); { run(); Thing::new(); { run(); } }\n\
               { use other::Thing as run; run::new(); }\n\
               { use other::*; run(); Thing::new(); }\n\
               { let serde = 1; serde::de::value(); }\n\
               { fn store() {} store::run(); }\n\
               { use other::*; nested::f(); { nested::f(); } }\n\
             }\n\
             struct Wrapper<T>(T);\n\
             impl<T: Into<Thing>> Wrapper<T> { fn f(&self) -> Option<Thing> { run(); \
               { Thing::new(); store::inner::run(); } None } }\n\
             fn generic<T: Default>() -> T { { { run(); } } store::run(); T::default() }\n\
             mod nested { pub fn f() {} }\n\
             fn opaque() { some_macro!(); store::run(); { store::run(); nested::f(); } }\n\
             mod hidden { #[unknown_attribute] fn f() {} fn g() { run(); super::run(); } }\n\
             #[cfg(test)]\n\
             mod tests {\n\
               use super::*;\n\
               fn t() { run(); Thing::new(); plain(); { run(); store::run(); } }\n\
               mod deeper { use super::*; fn u() { run(); plain(); { { run(); } } \
                 serde::de::value(); } }\n\
               mod globbed { use serde::*; fn v() { de::value(); run(); serde::de::value(); } }\n\
             }\n",
        ),
        (
            "src/store.rs",
            "pub fn run() {}\npub mod inner { pub fn run() {} }\n",
        ),
        (
            "src/other.rs",
            "pub struct Thing;\nimpl Thing { pub fn new() -> Self { { Thing } } }\n\
             pub mod nested { pub fn f() {} }\n\
             pub fn helper() { crate::run(); super::plain(); }\n",
        ),
    ];

    #[test]
    fn memos_change_no_answer_and_save_work_across_sites() {
        let snapshot = snapshot(FILES);
        let (cold, cold_work) = resolve_all(&snapshot, false);
        let (warm, warm_work) = resolve_all(&snapshot, true);
        assert_eq!(warm, cold);
        // The fixture repeats paths from the same scopes, so the memos are read.
        assert!(warm_work < cold_work, "{warm_work} >= {cold_work}");
        // The answers are the fixture's, not empty ones.
        let resolved = warm.iter().filter(|a| a.contains("native")).count();
        assert!(resolved > 10, "{warm:#?}");
        assert!(warm.iter().any(|a| a.contains("serde")), "{warm:#?}");
        assert!(warm.iter().any(|a| a.contains("rust_")), "{warm:#?}");
    }

    /// A walk is remembered for every scope it passes and a site's answer for the first
    /// scope that could bind its first segment; blocks that cannot are climbed, not keys.
    #[test]
    fn memos_are_keyed_by_the_scopes_that_can_bind() {
        let files = [(
            "src/lib.rs",
            "mod a { pub fn f() {} }\nfn f() { { { a::f(); } } a::f(); }\n\
             fn g() { use std::fmt as a; { a::f(); } }\n",
        )];
        let snapshot = snapshot(&files);
        let Read {
            docs,
            trees,
            packages,
            mut catalog,
        } = read(&snapshot);
        let mut check = || Ok(());
        let mut work = Work::new(work::reference_limit(0, 0));
        let sites = extract(docs[0], &trees[0], &mut work, &mut check).unwrap();
        let mut external_packages = BTreeMap::new();
        for site in &sites {
            catalog
                .resolve(
                    0,
                    site,
                    &packages,
                    &mut external_packages,
                    &docs,
                    &trees[0],
                    &mut check,
                )
                .unwrap();
        }
        let path = vec!["a".to_owned(), "f".to_owned()];
        let root = catalog.files[&0];
        // The two paths in `f` climb to the crate root; the one in `g` stops at `g`'s body,
        // whose import binds `a`.
        let starts: Vec<usize> = catalog
            .path_memo
            .keys()
            .map(|(start, key)| {
                assert_eq!(key, &path);
                *start
            })
            .collect();
        assert_eq!(starts.len(), 2, "{starts:?}");
        assert!(starts.contains(&root));
        assert!(starts.iter().any(|&start| start != root));
        // Each walk for `a` reached the root, and every scope it passed is remembered.
        assert!(
            catalog
                .prelude_memo
                .get(&(root, "a".to_owned(), false))
                .is_some_and(|&blocked| blocked)
        );
        assert!(catalog.prelude_memo.len() > 2);
    }

    /// The first proof passes in the first pass, its walk remembered as reaching the root;
    /// the second fails and turns the root opaque. The second pass must read the root as it
    /// now is: the first proof fails too and its scope turns opaque.
    #[test]
    fn a_failed_proof_pass_forgets_what_the_memos_read() {
        let files = [
            (
                "src/lib.rs",
                "mod store;\nfn first() {\n  #[tokio::main] async fn inner() {}\n  store::run();\n}\n\
             #[async_trait::async_trait] trait T { async fn f(&self); }\n",
            ),
            ("src/store.rs", "pub fn run() {}\n"),
        ];
        let snapshot = snapshot(&files);
        let Read {
            docs,
            trees,
            packages,
            mut catalog,
        } = read(&snapshot);
        assert_eq!(catalog.proofs.len(), 2);
        assert!(catalog.proofs.iter().all(|proof| proof.failed));
        assert!(catalog.scopes[catalog.proofs[0].scope].opaque);
        assert!(catalog.prelude_memo.is_empty());
        assert!(catalog.path_memo.is_empty());
        let mut check = || Ok(());
        let mut work = Work::new(work::reference_limit(0, 0));
        let sites = extract(docs[0], &trees[0], &mut work, &mut check).unwrap();
        let site = sites
            .iter()
            .find(|site| site.spelling == "store::run")
            .unwrap();
        let resolved = catalog
            .resolve(
                0,
                site,
                &packages,
                &mut BTreeMap::new(),
                &docs,
                &trees[0],
                &mut check,
            )
            .unwrap();
        assert!(resolved.targets.is_empty());
        assert!(resolved.residual.is_some());
    }
}

#[cfg(test)]
mod macro_rules_name_tests {
    use super::*;

    /// #1210: every name a definition can take, behind whitespace and (nested) comments and
    /// with `r#`; another macro's name ending in `macro_rules`, and a `macro_rules!` that no
    /// name follows, define nothing.
    #[test]
    fn macro_rules_names_reads_each_spelling_of_a_definition() {
        let text = "macro_rules! a {}\n\
                    macro_rules ! /* x /* nested */ */ b {}\n\
                    macro_rules!\n// comment\nr#c {}\n\
                    my_macro_rules! d {}\n\
                    \"macro_rules!\" e\n\
                    macro_rules!($x) {}\n\
                    macro_rules! /* unterminated";
        let names: Vec<String> = macro_rules_names(text).into_iter().collect();
        assert_eq!(names, ["a", "b", "c"]);
    }
}

#[cfg(test)]
mod feature_gate_tests {
    use super::feature_gate;

    #[test]
    fn a_gate_names_its_feature_by_the_literals_exact_value() {
        for (attribute, feature) in [
            ("#[cfg(feature=\"x\")]", Some("x")),
            ("#[cfg(feature = \"x\")]", Some("x")),
            ("#[ cfg ( feature\n=\t\"x\" ) ]", Some("x")),
            ("#![cfg(feature = \"test-support\")]", Some("test-support")),
            ("#[cfg(any(test, feature = \"x\"))]", Some("x")),
            ("#[cfg(any(feature = \"x\", test))]", Some("x")),
            // The value is compared as written: whitespace inside the quotes is part of it.
            ("#[cfg(feature = \"x y\")]", None),
            ("#[cfg(feature = \" x\")]", None),
            ("#[cfg(feature = \"x \")]", None),
            ("#[cfg(feature = \"test -support\")]", None),
            ("#[cfg(feature = \"\")]", None),
            // An escape, a raw literal and a comment are not interpreted.
            ("#[cfg(feature = \"\\u{78}\")]", None),
            ("#[cfg(feature = r\"x\")]", None),
            ("#[cfg(feature = r#\"x\"#)]", None),
            ("#[cfg(/* test */ feature = \"x\")]", None),
            // Only the two whole-condition forms count.
            ("#[cfg(not(feature = \"x\"))]", None),
            ("#[cfg(all(feature = \"x\"))]", None),
            ("#[cfg(all(test, feature = \"x\"))]", None),
            ("#[cfg(all(feature = \"x\", unix))]", None),
            ("#[cfg(any(feature = \"x\"))]", None),
            ("#[cfg(any(feature = \"x\", unix))]", None),
            ("#[cfg(any(test, feature = \"x\", unix))]", None),
            ("#[cfg(any(test, not(feature = \"x\")))]", None),
            ("#[cfg(any(feature = \"x\", feature = \"y\"))]", None),
            ("#[cfg(feature = \"x\", unix)]", None),
            ("#[cfg_attr(feature = \"x\", inline)]", None),
            ("#[cfg(target_feature = \"x\")]", None),
            ("#[cfg(feature = \"x\"", None),
            ("#[doc = \"feature = x\"]", None),
        ] {
            assert_eq!(feature_gate(attribute).as_deref(), feature, "{attribute}");
        }
    }
}
