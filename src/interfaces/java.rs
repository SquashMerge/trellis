//! Java HTTP interfaces (#857). Server routes are Spring MVC mapping annotations and JAX-RS
//! resources (`java_routes`); client calls are `java.net.http` requests, Spring `RestTemplate`
//! calls and `WebClient` requests (`java_calls`). This module parses each Java document once,
//! resolves the supertypes that route inheritance follows, and reads the string constants and
//! string expressions both sides share.
use super::rust_calls::UNKNOWN;
use super::{
    ClientCall, Handler, HandlerKind, LineIndex, Route, charge, comment, named_callable, parse,
    test_file,
};
use crate::native_evidence::Range;
use crate::references::{DocumentInput, Snapshot, walk, work::Work};
use crate::syntax_bounds::DEPTH;
use anyhow::Result;
use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tree_sitter::{Node, Tree};

/// How deep a format argument nests inside another before it is a substitution.
const FORMAT_DEPTH: usize = 4;
/// Ancestors a constant lookup climbs before it gives up.
const SCOPE_STEPS: usize = DEPTH * 4;
/// The type declaration kinds.
const TYPES: [&str; 5] = [
    "class_declaration",
    "interface_declaration",
    "enum_declaration",
    "record_declaration",
    "annotation_type_declaration",
];
const BODIES: [&str; 4] = [
    "class_body",
    "interface_body",
    "enum_body",
    "annotation_type_body",
];

/// The routes and client calls of every production Java document.
pub(super) fn extract(
    snapshot: &Snapshot,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<(Vec<Route>, Vec<ClientCall>)> {
    let mut world = World::new(snapshot);
    world.load(work, check)?;
    if world.servers.is_empty() && world.clients.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    world.resolve_all(work, check)?;
    let mut views = BTreeMap::new();
    for (index, tree) in world.trees.iter().enumerate() {
        if let Some(tree) = tree {
            check()?;
            let doc = &snapshot.documents[index];
            let view = Document::new(&world, index, doc, tree.root_node(), work, check)?;
            views.insert(index, view);
        }
    }
    let routes = super::java_routes::extract(&world, &views, work, check)?;
    let calls = super::java_calls::extract(&world.clients, &views, work, check)?;
    Ok((routes, calls))
}

/// Whether a Java document can register a Spring or JAX-RS route.
fn server_candidate(text: &str) -> bool {
    text.contains("Mapping") || text.contains("Controller") || text.contains("ws.rs")
}

/// Whether a Java document can make a `java.net.http`, `RestTemplate` or `WebClient` call.
fn client_candidate(text: &str) -> bool {
    text.contains("java.net.http")
        || text.contains("RestTemplate")
        || text.contains("RestOperations")
        || text.contains("WebClient")
}

/// A supertype as written, and what it resolved to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Super {
    /// `(document, type)`: an index into the snapshot's documents and that unit's types.
    Resolved(usize, usize),
    /// `java.lang.Object`, which carries no mappings.
    Object,
    /// Not bound to exactly one type of the snapshot: anything may come from it.
    Unresolved,
}

/// One type declaration of a parsed document.
pub(super) struct TypeInfo {
    pub(super) name: String,
    pub(super) kind: &'static str,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) top: bool,
    superclass_written: Option<String>,
    interfaces_written: Vec<String>,
    pub(super) superclass: Option<Super>,
    pub(super) interfaces: Vec<Super>,
}

/// The package, imports and types of one parsed document.
pub(super) struct Unit {
    package: String,
    imports: Vec<Import>,
    pub(super) types: Vec<TypeInfo>,
    /// Every annotation name written in the document.
    annotations_written: BTreeSet<String>,
    /// What each written annotation name resolves to, once resolved.
    annotations: BTreeMap<String, Super>,
}

#[derive(Clone)]
pub(super) struct Import {
    path: String,
    is_static: bool,
    wildcard: bool,
}

/// The parsed Java documents of a snapshot.
pub(super) struct World<'s> {
    snapshot: &'s Snapshot,
    trees: Vec<Option<Tree>>,
    pub(super) units: BTreeMap<usize, Unit>,
    /// Production Java documents by file name, for type lookup.
    files: HashMap<&'s str, Vec<usize>>,
    pub(super) servers: Vec<usize>,
    pub(super) clients: Vec<usize>,
}

impl<'s> World<'s> {
    fn new(snapshot: &'s Snapshot) -> Self {
        Self {
            snapshot,
            trees: (0..snapshot.documents.len()).map(|_| None).collect(),
            units: BTreeMap::new(),
            files: HashMap::new(),
            servers: Vec::new(),
            clients: Vec::new(),
        }
    }

    /// Parses every server and client candidate.
    fn load(&mut self, work: &mut Work, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
        for (index, doc) in self.snapshot.documents.iter().enumerate() {
            check()?;
            if doc.key.language != "java"
                || self.snapshot.evidence_gaps.contains_key(&doc.key)
                || test_file(doc)
            {
                continue;
            }
            let name = doc.key.path.rsplit('/').next().unwrap_or(&doc.key.path);
            self.files.entry(name).or_default().push(index);
            let text = &doc.file.text;
            let (server, client) = (server_candidate(text), client_candidate(text));
            if server {
                self.servers.push(index);
            }
            if client {
                self.clients.push(index);
            }
            if server || client {
                self.parse(index, work, check)?;
            }
        }
        Ok(())
    }

    /// Parses one document once, charging its text, and records its unit.
    fn parse(
        &mut self,
        index: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        if self.trees[index].is_some() {
            return Ok(());
        }
        let doc = &self.snapshot.documents[index];
        charge(work, doc.file.text.len())?;
        let tree = parse(doc, &tree_sitter_java::LANGUAGE.into(), check)?;
        let unit = unit(doc, tree.root_node(), check)?;
        self.units.insert(index, unit);
        self.trees[index] = Some(tree);
        Ok(())
    }

    /// Resolves the supertypes of every server candidate's types, and of each type they
    /// resolve to, parsing the documents that declare them.
    fn resolve_all(
        &mut self,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let mut queue: Vec<usize> = self.servers.clone();
        let mut done = BTreeSet::new();
        while let Some(index) = queue.pop() {
            if !done.insert(index) {
                continue;
            }
            let count = self.units.get(&index).map_or(0, |unit| unit.types.len());
            for at in 0..count {
                check()?;
                let (superclass, interfaces) = {
                    let info = &self.units[&index].types[at];
                    (
                        info.superclass_written.clone(),
                        info.interfaces_written.clone(),
                    )
                };
                charge(work, 1 + interfaces.len())?;
                let superclass = match superclass {
                    Some(written) => Some(self.resolve(index, &written, &mut queue, work, check)?),
                    None => None,
                };
                let mut resolved = Vec::with_capacity(interfaces.len());
                for written in &interfaces {
                    resolved.push(self.resolve(index, written, &mut queue, work, check)?);
                }
                let info = &mut self.units.get_mut(&index).expect("parsed unit").types[at];
                info.superclass = superclass;
                info.interfaces = resolved;
            }
            // Annotation types, so that a composed annotation declared in the snapshot is
            // known for what it carries (#876 review).
            let written: Vec<String> = self
                .units
                .get(&index)
                .map(|unit| unit.annotations_written.iter().cloned().collect())
                .unwrap_or_default();
            charge(work, written.len())?;
            for name in written {
                check()?;
                let resolved = self.resolve(index, &name, &mut queue, work, check)?;
                if let Some(unit) = self.units.get_mut(&index) {
                    unit.annotations.insert(name, resolved);
                }
            }
        }
        Ok(())
    }

    /// The type an annotation name written in a document resolved to.
    pub(super) fn annotation(&self, index: usize, written: &str) -> Super {
        self.units
            .get(&index)
            .and_then(|unit| unit.annotations.get(written))
            .copied()
            .unwrap_or(Super::Unresolved)
    }

    /// Whether a file `Name.java` of the document's own package is in the snapshot: a type of
    /// the same package that shadows an on-demand or `java.lang` name.
    pub(super) fn same_package_file(&self, index: usize, simple: &str) -> bool {
        let package = self
            .units
            .get(&index)
            .map_or("", |unit| unit.package.as_str());
        let file = format!("{simple}.java");
        let tail = if package.is_empty() {
            file.clone()
        } else {
            format!("{}/{file}", package.replace('.', "/"))
        };
        self.files
            .get(file.as_str())
            .into_iter()
            .flatten()
            .any(|&other| {
                let path = &self.snapshot.documents[other].key.path;
                path == &tail || path.ends_with(&format!("/{tail}"))
            })
    }

    /// A parameter type written in a document, qualified so that two distinct types never
    /// compare equal and one type always reads the same (#876 review): a package-qualified
    /// name as written; a type the document declares once, by its package and enclosing
    /// types; a single-type import; a type of the same package in its own file; a known
    /// `java.lang` type. Anything else (a name the document declares twice, a qualified
    /// name that starts with a type, a non-public type of another file, a type variable, a
    /// name an on-demand import may supply) is uncertain (`?`) and equals only itself.
    pub(super) fn qualify(&self, index: usize, written: &str) -> String {
        let mut base = String::new();
        let mut depth = 0usize;
        for c in written.chars() {
            match c {
                '<' => depth += 1,
                '>' => depth = depth.saturating_sub(1),
                c if c.is_whitespace() => {}
                c if depth == 0 => base.push(c),
                _ => {}
            }
        }
        let (name, suffix) = match base.find('[') {
            Some(at) => base.split_at(at),
            None => (base.as_str(), ""),
        };
        const PRIMITIVES: [&str; 9] = [
            "boolean", "byte", "char", "short", "int", "long", "float", "double", "void",
        ];
        let Some(unit) = self.units.get(&index) else {
            return format!("?{index}.{name}{suffix}");
        };
        // `java.lang` types an on-demand import cannot also supply without ambiguity.
        const LANG: [&str; 26] = [
            "Boolean",
            "Byte",
            "Character",
            "CharSequence",
            "Class",
            "Comparable",
            "Double",
            "Enum",
            "Exception",
            "Float",
            "Integer",
            "Iterable",
            "Long",
            "Number",
            "Object",
            "Record",
            "Runnable",
            "RuntimeException",
            "Short",
            "String",
            "StringBuilder",
            "Thread",
            "Throwable",
            "Void",
            "Math",
            "Error",
        ];
        let declared: Vec<&TypeInfo> = unit.types.iter().filter(|info| info.name == name).collect();
        let qualified = if PRIMITIVES.contains(&name) {
            name.to_owned()
        } else if name.contains('.') {
            // A package-qualified name is canonical as written. One that starts with a type
            // name (`Outer.Inner`) is not proven to be, so it is uncertain.
            if name.starts_with(|c: char| c.is_ascii_uppercase()) {
                format!("?{index}.{name}")
            } else {
                name.to_owned()
            }
        } else if let [info] = declared[..] {
            // The canonical name of a type the document declares: the package, the enclosing
            // types, then the name (#876 review), as a qualified or imported spelling of it
            // in another document reads.
            let mut enclosing: Vec<&TypeInfo> = unit
                .types
                .iter()
                .filter(|outer| {
                    outer.start <= info.start && info.end <= outer.end && outer.start != info.start
                })
                .collect();
            enclosing.sort_by_key(|outer| outer.start);
            let mut parts: Vec<&str> = Vec::new();
            if !unit.package.is_empty() {
                parts.push(&unit.package);
            }
            parts.extend(enclosing.iter().map(|outer| outer.name.as_str()));
            parts.push(name);
            parts.join(".")
        } else if !declared.is_empty() {
            format!("?{index}.{name}")
        } else if let Some(import) = unit.imports.iter().find(|import| {
            !import.is_static && !import.wildcard && import.path.rsplit('.').next() == Some(name)
        }) {
            import.path.clone()
        } else if self.same_package_file(index, name) {
            format!("{}.{name}", unit.package)
        } else if LANG.contains(&name) {
            format!("java.lang.{name}")
        } else {
            // Not proven: a non-public type of another file, a type variable, or a type an
            // on-demand import supplies.
            format!("?{index}.{name}")
        };
        format!("{qualified}{suffix}")
    }

    /// The one type a supertype name written in `from` binds to: a type of the same document,
    /// else a single-type import, else a type of the same package. An on-demand import, a
    /// nested qualified name, or a name with no single declaration is unresolved.
    fn resolve(
        &mut self,
        from: usize,
        written: &str,
        queue: &mut Vec<usize>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Super> {
        let Some(name) = type_name(written) else {
            return Ok(Super::Unresolved);
        };
        if name.contains('.') {
            let found = self.lookup(&name, queue, work, check)?;
            // Only a proven `java.lang.Object` is mapping-free (#876 review).
            return Ok(match found {
                Super::Unresolved if name == "java.lang.Object" => Super::Object,
                found => found,
            });
        }
        let unit = &self.units[&from];
        let mut same = unit
            .types
            .iter()
            .enumerate()
            .filter(|(_, info)| info.name == name);
        match (same.next(), same.next()) {
            (Some((at, _)), None) => return Ok(Super::Resolved(from, at)),
            (Some(_), Some(_)) => return Ok(Super::Unresolved),
            _ => {}
        }
        let suffix = format!(".{name}");
        let imported: Vec<String> = unit
            .imports
            .iter()
            .filter(|import| {
                !import.is_static && !import.wildcard && import.path.ends_with(&suffix)
            })
            .map(|import| import.path.clone())
            .collect();
        let qualified = match &imported[..] {
            [only] => only.clone(),
            [] if name == "Object" && !self.same_package_file(from, &name) => {
                // Neither declared, imported nor in the package: `java.lang.Object`, which
                // an on-demand import cannot also supply without ambiguity.
                return Ok(Super::Object);
            }
            [] if unit.package.is_empty() => name,
            [] => format!("{}.{name}", unit.package),
            _ => return Ok(Super::Unresolved),
        };
        self.lookup(&qualified, queue, work, check)
    }

    /// The one top-level type `package.Name` declared in a file `Name.java` under the
    /// package's directories.
    fn lookup(
        &mut self,
        qualified: &str,
        queue: &mut Vec<usize>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Super> {
        let (package, simple) = qualified.rsplit_once('.').unwrap_or(("", qualified));
        let file = format!("{simple}.java");
        let tail = if package.is_empty() {
            file.clone()
        } else {
            format!("{}/{file}", package.replace('.', "/"))
        };
        let candidates: Vec<usize> = self
            .files
            .get(file.as_str())
            .into_iter()
            .flatten()
            .copied()
            .filter(|&index| {
                let path = &self.snapshot.documents[index].key.path;
                path == &tail || path.ends_with(&format!("/{tail}"))
            })
            .collect();
        charge(work, candidates.len())?;
        let mut found = Vec::new();
        for index in candidates {
            check()?;
            self.parse(index, work, check)?;
            queue.push(index);
            let unit = &self.units[&index];
            if unit.package != package {
                continue;
            }
            for (at, info) in unit.types.iter().enumerate() {
                if info.top && info.name == simple {
                    found.push((index, at));
                }
            }
        }
        Ok(match found[..] {
            [(index, at)] => Super::Resolved(index, at),
            _ => Super::Unresolved,
        })
    }

    /// The type declaration node of a unit's type.
    pub(super) fn node<'v, 't>(
        &self,
        views: &'v Views<'t>,
        index: usize,
        at: usize,
    ) -> Option<(&'v Document<'t>, Node<'t>)> {
        let view = views.get(&index)?;
        let info = self.units.get(&index)?.types.get(at)?;
        let mut node = view.root.descendant_for_byte_range(info.start, info.end)?;
        loop {
            if node.kind() == info.kind
                && node.start_byte() == info.start
                && node.end_byte() == info.end
            {
                return Some((view, node));
            }
            node = node.parent()?;
        }
    }
}

pub(super) type Views<'t> = BTreeMap<usize, Document<'t>>;

/// A written type without type arguments, annotations or whitespace. `None` when it carries
/// an annotation.
fn type_name(written: &str) -> Option<String> {
    let mut out = String::new();
    let mut depth = 0usize;
    for c in written.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.checked_sub(1)?,
            '@' => return None,
            c if c.is_whitespace() => {}
            c if depth == 0 => out.push(c),
            _ => {}
        }
    }
    (!out.is_empty() && depth == 0).then_some(out)
}

/// The package, imports and type declarations of one document.
fn unit(
    doc: &DocumentInput,
    root: Node<'_>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Unit> {
    let text = |node: Node<'_>| doc.file.text.get(node.byte_range()).unwrap_or("");
    let mut package = String::new();
    let mut imports = Vec::new();
    let mut cursor = root.walk();
    for child in root.named_children(&mut cursor) {
        match child.kind() {
            "package_declaration" => {
                let mut inner = child.walk();
                if let Some(name) = child
                    .named_children(&mut inner)
                    .find(|n| matches!(n.kind(), "identifier" | "scoped_identifier"))
                {
                    package = text(name).split_whitespace().collect();
                }
            }
            "import_declaration" => imports.push(import(child, &text)),
            _ => {}
        }
    }
    let mut types = Vec::new();
    let mut annotations_written = BTreeSet::new();
    walk(root, check, &mut |node, _| {
        if matches!(node.kind(), "annotation" | "marker_annotation")
            && let Some(name) = node.child_by_field_name("name")
        {
            annotations_written.insert(text(name).split_whitespace().collect::<String>());
        }
        let Some(kind) = TYPES.iter().copied().find(|kind| *kind == node.kind()) else {
            return Ok(false);
        };
        let Some(name) = node.child_by_field_name("name") else {
            return Ok(false);
        };
        let list = |list: Node<'_>| -> Vec<String> {
            let mut written = Vec::new();
            let mut cursor = list.walk();
            for child in list.named_children(&mut cursor) {
                if child.kind() == "type_list" {
                    let mut inner = child.walk();
                    written.extend(
                        child
                            .named_children(&mut inner)
                            .filter(|n| !comment(*n))
                            .map(|n| text(n).to_owned()),
                    );
                }
            }
            written
        };
        let superclass_written = node
            .child_by_field_name("superclass")
            .and_then(|superclass| {
                let mut cursor = superclass.walk();
                superclass
                    .named_children(&mut cursor)
                    .find(|n| !comment(*n))
                    .map(|n| text(n).to_owned())
            });
        let mut interfaces_written = Vec::new();
        if let Some(interfaces) = node.child_by_field_name("interfaces") {
            interfaces_written = list(interfaces);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if child.kind() == "extends_interfaces" {
                interfaces_written.extend(list(child));
            }
        }
        types.push(TypeInfo {
            name: text(name).to_owned(),
            kind,
            start: node.start_byte(),
            end: node.end_byte(),
            top: node
                .parent()
                .is_some_and(|parent| parent.kind() == "program"),
            superclass_written,
            interfaces_written,
            superclass: None,
            interfaces: Vec::new(),
        });
        Ok(false)
    })?;
    Ok(Unit {
        package,
        imports,
        types,
        annotations_written,
        annotations: BTreeMap::new(),
    })
}

fn import<'a>(node: Node<'_>, text: &impl Fn(Node<'_>) -> &'a str) -> Import {
    let mut is_static = false;
    let mut wildcard = false;
    let mut path = String::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "static" => is_static = true,
            "asterisk" => wildcard = true,
            "identifier" | "scoped_identifier" => {
                path = text(child).split_whitespace().collect();
            }
            _ => {}
        }
    }
    Import {
        path,
        is_static,
        wildcard,
    }
}

/// A field that may be a readable constant: its owner's node, its value and its written type.
type Field<'a> = (usize, Node<'a>, &'a str);

/// One declaration of a variable name: a field, local, parameter or other binding.
pub(super) struct Binding<'a> {
    pub(super) ty: Option<Node<'a>>,
    pub(super) value: Option<Node<'a>>,
    /// A parameter or other binding whose value the document does not show.
    pub(super) parameter: bool,
}

/// One parsed Java document, with its imports, constants and variable bindings.
pub(super) struct Document<'a> {
    pub(super) doc: &'a DocumentInput,
    pub(super) source: &'a str,
    pub(super) root: Node<'a>,
    pub(super) lines: LineIndex,
    keys: OnceCell<BTreeMap<String, String>>,
    world: &'a World<'a>,
    index: usize,
    imports: Vec<Import>,
    /// Every name a local, parameter, pattern, lambda, catch, resource, `for` variable or
    /// single static import binds.
    bound: BTreeSet<String>,
    /// The simple names of the types the document declares.
    types: BTreeSet<&'a str>,
    /// Readable constants by name: the declaring type's node and the value.
    constants: BTreeMap<&'a str, (usize, Cow<'a, str>)>,
    /// Every declaration of each variable name.
    pub(super) declared: BTreeMap<&'a str, Vec<Binding<'a>>>,
    /// The right-hand side of each `name = e` and `this.name = e`.
    pub(super) assigned: BTreeMap<&'a str, Vec<Node<'a>>>,
}

impl<'a> Document<'a> {
    /// Reads the document's bindings in one walk, charged as a scan of its text. A name is a
    /// readable constant only when it is declared exactly once in the document, as a
    /// `static final String` field (or an interface field) initialised with a plain string
    /// literal, and nothing else in the document binds it: no local, parameter, catch,
    /// resource, `for`, lambda or pattern variable, enum constant, other field, or single
    /// static import.
    fn new(
        world: &'a World<'a>,
        index: usize,
        doc: &'a DocumentInput,
        root: Node<'a>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Self> {
        charge(work, doc.file.text.len())?;
        let source: &'a str = &doc.file.text;
        let text = |node: Node<'_>| source.get(node.byte_range()).unwrap_or("");
        let mut imports = Vec::new();
        let mut types = BTreeSet::new();
        let mut bound: BTreeSet<String> = BTreeSet::new();
        let mut fields: BTreeMap<&'a str, Vec<Option<Field<'a>>>> = BTreeMap::new();
        let mut declared: BTreeMap<&'a str, Vec<Binding<'a>>> = BTreeMap::new();
        let mut assigned: BTreeMap<&'a str, Vec<Node<'a>>> = BTreeMap::new();
        walk(root, check, &mut |node, _| {
            match node.kind() {
                "import_declaration" => {
                    let import = import(node, &text);
                    if import.is_static
                        && !import.wildcard
                        && let Some(name) = import.path.rsplit('.').next()
                    {
                        bound.insert(name.to_owned());
                    }
                    imports.push(import);
                }
                kind if TYPES.contains(&kind) => {
                    if let Some(name) = node.child_by_field_name("name") {
                        types.insert(text(name));
                    }
                }
                "field_declaration" | "constant_declaration" => {
                    let owner = owner(node);
                    let implicit = node.kind() == "constant_declaration"
                        || node.parent().is_some_and(|parent| {
                            matches!(parent.kind(), "interface_body" | "annotation_type_body")
                        });
                    let constant =
                        implicit || (has_modifier(node, "static") && has_modifier(node, "final"));
                    let ty = node.child_by_field_name("type");
                    let written = ty.map(text);
                    let mut cursor = node.walk();
                    for declarator in node.children_by_field_name("declarator", &mut cursor) {
                        let Some(name) = declarator.child_by_field_name("name") else {
                            continue;
                        };
                        let value = declarator.child_by_field_name("value");
                        let readable = match (owner, value, written) {
                            (Some(owner), Some(value), Some(written))
                                if constant && value.kind() == "string_literal" =>
                            {
                                Some((owner, value, written))
                            }
                            _ => None,
                        };
                        fields.entry(text(name)).or_default().push(readable);
                        declared.entry(text(name)).or_default().push(Binding {
                            ty,
                            value,
                            parameter: false,
                        });
                    }
                }
                "enum_constant" => {
                    // An enum constant is a field: it binds a value name (#876 review).
                    if let Some(name) = node.child_by_field_name("name") {
                        fields.entry(text(name)).or_default().push(None);
                        bound.insert(text(name).to_owned());
                    }
                }
                "local_variable_declaration" => {
                    let ty = node.child_by_field_name("type");
                    let mut cursor = node.walk();
                    for declarator in node.children_by_field_name("declarator", &mut cursor) {
                        if let Some(name) = declarator.child_by_field_name("name") {
                            bound.insert(text(name).to_owned());
                            declared.entry(text(name)).or_default().push(Binding {
                                ty,
                                value: declarator.child_by_field_name("value"),
                                parameter: false,
                            });
                        }
                    }
                }
                "formal_parameter"
                | "catch_formal_parameter"
                | "enhanced_for_statement"
                | "resource" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        bound.insert(text(name).to_owned());
                        declared.entry(text(name)).or_default().push(Binding {
                            ty: node.child_by_field_name("type"),
                            value: None,
                            parameter: true,
                        });
                    }
                }
                "spread_parameter" => {
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        if child.kind() == "variable_declarator"
                            && let Some(name) = child.child_by_field_name("name")
                        {
                            bound.insert(text(name).to_owned());
                            declared.entry(text(name)).or_default().push(Binding {
                                ty: None,
                                value: None,
                                parameter: true,
                            });
                        }
                    }
                }
                "lambda_expression" => {
                    if let Some(parameters) = node.child_by_field_name("parameters") {
                        let names: Vec<Node<'a>> = match parameters.kind() {
                            "identifier" => vec![parameters],
                            "inferred_parameters" => {
                                let mut cursor = parameters.walk();
                                parameters
                                    .named_children(&mut cursor)
                                    .filter(|n| n.kind() == "identifier")
                                    .collect()
                            }
                            _ => Vec::new(),
                        };
                        for name in names {
                            bound.insert(text(name).to_owned());
                            declared.entry(text(name)).or_default().push(Binding {
                                ty: None,
                                value: None,
                                parameter: true,
                            });
                        }
                    }
                }
                "instanceof_expression" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        bound.insert(text(name).to_owned());
                    }
                }
                "type_pattern" | "record_pattern_component" => {
                    let mut cursor = node.walk();
                    for child in node.named_children(&mut cursor) {
                        if child.kind() == "identifier" {
                            bound.insert(text(child).to_owned());
                        }
                    }
                }
                "assignment_expression" => {
                    let operator = node.child_by_field_name("operator").map(|op| op.kind());
                    if let (Some("="), Some(left), Some(right)) = (
                        operator,
                        node.child_by_field_name("left"),
                        node.child_by_field_name("right"),
                    ) && let Some(name) = assigned_name(left, &text)
                    {
                        assigned.entry(name).or_default().push(right);
                    }
                }
                _ => {}
            }
            Ok(false)
        })?;
        let mut view = Self {
            doc,
            source,
            root,
            lines: LineIndex::new(source),
            keys: OnceCell::new(),
            world,
            index,
            imports,
            bound,
            types,
            constants: BTreeMap::new(),
            declared,
            assigned,
        };
        let mut constants = BTreeMap::new();
        for (name, declarations) in fields {
            if view.bound.contains(name) {
                continue;
            }
            if let [Some((owner, value, written))] = declarations[..]
                && view.string_type(written)
                && let Some(literal) = literal_text(value, source)
            {
                constants.insert(name, (owner, literal));
            }
        }
        view.constants = constants;
        Ok(view)
    }

    /// Whether a type name written simple is the `java.lang` type: declared nowhere in the
    /// document, imported singly from nowhere else, and no file of the same package declares
    /// it. An on-demand import cannot also supply it without ambiguity.
    fn java_lang(&self, simple: &str) -> bool {
        let suffix = format!(".{simple}");
        !self.types.contains(simple)
            && !self.imports.iter().any(|import| {
                !import.is_static
                    && !import.wildcard
                    && import.path.ends_with(&suffix)
                    && import.path != format!("java.lang{suffix}")
            })
            && !self.world.same_package_file(self.index, simple)
    }

    /// Whether a written type is `java.lang.String`.
    pub(super) fn string_type(&self, written: &str) -> bool {
        let written: String = written.split_whitespace().collect();
        written == "java.lang.String" || (written == "String" && self.java_lang("String"))
    }

    /// Whether a name is bound as a value anywhere in the document, so that in an
    /// expression it may name a value rather than a type: a field or enum constant, a record
    /// component, a local, a method, constructor, lambda or catch parameter, a pattern
    /// variable, a `for` or try-with-resources variable, or a single static import.
    fn value_name(&self, name: &str) -> bool {
        self.bound.contains(name) || self.declared.contains_key(name)
    }

    /// As [`Self::resolves`], for a type written as an expression's qualifier, such as the
    /// `HttpMethod` of `HttpMethod.GET`: a variable of the first name would be read instead,
    /// so none may be bound anywhere in the document (#876 review).
    pub(super) fn resolves_value(&self, written: &str, packages: &[&str], simple: &str) -> bool {
        let written: String = written.split_whitespace().collect();
        let first = written.split('.').next().unwrap_or_default();
        !self.value_name(first) && self.resolves(&written, packages, simple)
    }

    pub(super) fn text(&self, node: Node<'_>) -> &'a str {
        self.source.get(node.byte_range()).unwrap_or("")
    }

    /// The node's text without whitespace.
    pub(super) fn compact(&self, node: Node<'_>) -> String {
        self.text(node).split_whitespace().collect()
    }

    pub(super) fn named_children(node: Node<'a>) -> Vec<Node<'a>> {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| !comment(*child))
            .collect()
    }

    pub(super) fn arguments(call: Node<'a>) -> Vec<Node<'a>> {
        call.child_by_field_name("arguments")
            .map(Self::named_children)
            .unwrap_or_default()
    }

    pub(super) fn range(node: Node<'_>) -> Range {
        Range {
            start: node.start_byte(),
            end: node.end_byte(),
        }
    }

    /// Whether a written type or annotation name is `simple` from one of `packages`: written
    /// fully qualified, or written simple with a single-type or on-demand import of it and no
    /// type of that name declared in the document.
    pub(super) fn resolves(&self, written: &str, packages: &[&str], simple: &str) -> bool {
        let written: String = written.split_whitespace().collect();
        if packages
            .iter()
            .any(|package| written == format!("{package}.{simple}"))
        {
            return true;
        }
        if written != simple || self.types.contains(simple) {
            return false;
        }
        let suffix = format!(".{simple}");
        let mut single = self
            .imports
            .iter()
            .filter(|import| {
                !import.is_static && !import.wildcard && import.path.ends_with(&suffix)
            })
            .peekable();
        if single.peek().is_some() {
            return single.all(|import| {
                packages
                    .iter()
                    .any(|package| import.path == format!("{package}{suffix}"))
            });
        }
        // A type of the document's own package shadows an on-demand import.
        !self.world.same_package_file(self.index, simple)
            && self.imports.iter().any(|import| {
                !import.is_static && import.wildcard && packages.contains(&import.path.as_str())
            })
    }

    /// The value of a readable constant named at `at`, when the declaring type encloses `at`
    /// and no type between them has a supertype that could declare the same name.
    pub(super) fn constant(&self, name: &str, at: Node<'_>) -> Option<&str> {
        let (owner, value) = self.constants.get(name)?;
        let mut node = at;
        for _ in 0..SCOPE_STEPS {
            let parent = node.parent()?;
            if BODIES.contains(&parent.kind()) {
                let declaration = parent.parent()?;
                if declaration.id() == *owner {
                    return Some(value.as_ref());
                }
                if inherits(declaration) {
                    return None;
                }
            }
            node = parent;
        }
        None
    }

    /// A string literal with no escapes, as its content.
    pub(super) fn literal(&self, node: Node<'_>) -> Option<Cow<'a, str>> {
        literal_text(node, self.source)
    }

    /// A string expression with [`UNKNOWN`] for each part that cannot be read: string
    /// literals, visible constants, `+` concatenation, `String.format(..)` and
    /// `"..".formatted(..)`. Each part is charged before it is copied.
    pub(super) fn string(
        &self,
        node: Node<'a>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<String> {
        self.string_at(node, 0, work, check)
    }

    /// A string expression that must be read completely, such as an annotation value.
    pub(super) fn strict(
        &self,
        node: Node<'a>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        let text = self.string(node, work, check)?;
        Ok((!text.contains(UNKNOWN)).then_some(text))
    }

    fn string_at(
        &self,
        node: Node<'a>,
        depth: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<String> {
        let mut out = String::new();
        let mut stack = vec![node];
        while let Some(node) = stack.pop() {
            check()?;
            charge(work, 1)?;
            let node = unparenthesize(node);
            match node.kind() {
                "binary_expression"
                    if node
                        .child_by_field_name("operator")
                        .is_some_and(|op| op.kind() == "+") =>
                {
                    match (
                        node.child_by_field_name("left"),
                        node.child_by_field_name("right"),
                    ) {
                        (Some(left), Some(right)) => {
                            stack.push(right);
                            stack.push(left);
                        }
                        _ => out.push(UNKNOWN),
                    }
                }
                "string_literal" => match self.literal(node) {
                    Some(text) => {
                        charge(work, text.len())?;
                        out.push_str(&text);
                    }
                    None => out.push(UNKNOWN),
                },
                "identifier" => match self.constant(self.text(node), node) {
                    Some(text) => {
                        charge(work, text.len())?;
                        out.push_str(text);
                    }
                    None => out.push(UNKNOWN),
                },
                "method_invocation" if depth < FORMAT_DEPTH => {
                    match self.format(node, depth, work, check)? {
                        Some(text) => {
                            charge(work, text.len())?;
                            out.push_str(&text);
                        }
                        None => out.push(UNKNOWN),
                    }
                }
                _ => out.push(UNKNOWN),
            }
        }
        Ok(out)
    }

    /// `String.format(template, args..)` or `template.formatted(args..)`. A plain `%s` takes
    /// its argument's text; any other conversion, or a `%s` with an index, flag, width or
    /// precision, is a substitution. `None` when the template cannot be read.
    fn format(
        &self,
        call: Node<'a>,
        depth: usize,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        let name = call.child_by_field_name("name").map(|n| self.text(n));
        let object = call.child_by_field_name("object");
        let arguments = Self::arguments(call);
        let (template, values) = match (name, object) {
            (Some("format"), Some(object))
                if !self.value_name(self.compact(object).split('.').next().unwrap_or_default())
                    && self.string_type(&self.compact(object)) =>
            {
                match arguments.split_first() {
                    Some((template, values)) => (*template, values.to_vec()),
                    None => return Ok(None),
                }
            }
            (Some("formatted"), Some(object)) => (object, arguments),
            _ => return Ok(None),
        };
        let template = self.string_at(template, depth + 1, work, check)?;
        if template.contains(UNKNOWN) {
            return Ok(None);
        }
        let mut read: Vec<Option<String>> = vec![None; values.len()];
        let mut next = 0;
        let mut out = String::new();
        let mut chars = template.chars();
        while let Some(c) = chars.next() {
            if c != '%' {
                out.push(c);
                continue;
            }
            check()?;
            let mut spec = String::new();
            let conversion = loop {
                match chars.next() {
                    None => return Ok(None),
                    Some(c) if c.is_ascii_alphabetic() || c == '%' => break c,
                    Some(c) => spec.push(c),
                }
            };
            if matches!(conversion, 't' | 'T') && chars.next().is_none() {
                return Ok(None);
            }
            match conversion {
                '%' if spec.is_empty() => out.push('%'),
                'n' if spec.is_empty() => out.push(UNKNOWN),
                's' if spec.is_empty() => {
                    let at = next;
                    next += 1;
                    let Some(value) = values.get(at) else {
                        return Ok(None);
                    };
                    if read[at].is_none() {
                        read[at] = Some(self.string_at(*value, depth + 1, work, check)?);
                    }
                    let text = read[at].as_deref().unwrap_or_default();
                    charge(work, text.len())?;
                    out.push_str(text);
                }
                _ => {
                    // An explicit index or `<` reuses an argument; any other conversion takes
                    // the next one.
                    if !(spec.contains('$') || spec.starts_with('<')) {
                        next += 1;
                    }
                    out.push(UNKNOWN);
                }
            }
        }
        Ok(Some(out))
    }

    /// The annotations on a declaration's modifiers, in written order.
    pub(super) fn annotations(declaration: Node<'a>) -> Vec<Node<'a>> {
        let mut found = Vec::new();
        let mut cursor = declaration.walk();
        for child in declaration.named_children(&mut cursor) {
            if child.kind() == "modifiers" {
                let mut inner = child.walk();
                found.extend(
                    child
                        .named_children(&mut inner)
                        .filter(|n| matches!(n.kind(), "annotation" | "marker_annotation")),
                );
            }
        }
        found
    }

    /// The written name of an annotation.
    pub(super) fn annotation_name(&self, annotation: Node<'_>) -> String {
        annotation
            .child_by_field_name("name")
            .map(|name| self.compact(name))
            .unwrap_or_default()
    }

    /// An annotation's unnamed value, and its `key = value` pairs.
    pub(super) fn annotation_arguments(annotation: Node<'a>) -> AnnotationArguments<'a> {
        let mut arguments = AnnotationArguments::default();
        let Some(list) = annotation.child_by_field_name("arguments") else {
            return arguments;
        };
        for child in Self::named_children(list) {
            if child.kind() == "element_value_pair" {
                if let (Some(key), Some(value)) = (
                    child.child_by_field_name("key"),
                    child.child_by_field_name("value"),
                ) {
                    arguments.pairs.push((key, value));
                }
            } else {
                arguments.values.push(child);
            }
        }
        arguments
    }

    /// The elements of an annotation value: those of an array initializer, or the value.
    pub(super) fn elements(value: Node<'a>) -> Vec<Node<'a>> {
        if value.kind() == "element_value_array_initializer" {
            Self::named_children(value)
        } else {
            vec![value]
        }
    }

    fn keys(&self) -> &BTreeMap<String, String> {
        self.keys.get_or_init(|| {
            crate::symbol_lookup::keys_from_source(
                &self.doc.key.path,
                &self.doc.key.language,
                self.source,
                &self.doc.declarations,
            )
            .unwrap_or_default()
        })
    }

    /// The handler of a method declaration: the method's declaration, or the span inside
    /// the named callable around it.
    pub(super) fn handler(&self, method: Node<'_>) -> Handler {
        let range = Self::range(method);
        let name = method.child_by_field_name("name").map(|n| self.text(n));
        let declaration = self
            .doc
            .declarations
            .iter()
            .filter(|d| {
                d.kind == "method"
                    && d.name.as_deref() == name
                    && range.start <= d.range.start
                    && d.range.end <= range.end
            })
            .max_by_key(|d| d.range.end - d.range.start);
        match declaration {
            Some(declaration) => Handler {
                kind: HandlerKind::Declaration,
                syntax_id: Some(declaration.syntax_id.clone()),
                stable_lookup_key: self.keys().get(&declaration.syntax_id).cloned(),
                document: self.doc.key.clone(),
                lines: self.lines.lines(&declaration.range),
                range: declaration.range.clone(),
                variant: None,
            },
            None => {
                let callable = named_callable(self.doc, &range);
                Handler {
                    kind: HandlerKind::Closure,
                    syntax_id: callable.map(|d| d.syntax_id.clone()),
                    stable_lookup_key: callable
                        .and_then(|d| self.keys().get(&d.syntax_id).cloned()),
                    document: self.doc.key.clone(),
                    lines: self.lines.lines(&range),
                    range,
                    variant: None,
                }
            }
        }
    }
}

#[derive(Default)]
pub(super) struct AnnotationArguments<'a> {
    pub(super) values: Vec<Node<'a>>,
    pub(super) pairs: Vec<(Node<'a>, Node<'a>)>,
}

/// Whether a declaration's modifiers include the keyword.
pub(super) fn has_modifier(declaration: Node<'_>, keyword: &str) -> bool {
    let mut cursor = declaration.walk();
    let modifiers: Vec<Node<'_>> = declaration
        .named_children(&mut cursor)
        .filter(|child| child.kind() == "modifiers")
        .collect();
    modifiers.into_iter().any(|modifiers| {
        let mut inner = modifiers.walk();
        modifiers
            .children(&mut inner)
            .any(|n| !n.is_named() && n.kind() == keyword)
    })
}

/// The node of the type whose body directly declares a field: a type declaration, or an
/// anonymous class's creation expression.
fn owner(field: Node<'_>) -> Option<usize> {
    let mut body = field.parent()?;
    if body.kind() == "enum_body_declarations" {
        body = body.parent()?;
    }
    BODIES
        .contains(&body.kind())
        .then(|| body.parent())
        .flatten()
        .map(|declaration| declaration.id())
}

/// Whether a type, or an anonymous class, has a supertype that may declare a field.
fn inherits(declaration: Node<'_>) -> bool {
    if declaration.kind() == "object_creation_expression" {
        return true;
    }
    if declaration.child_by_field_name("superclass").is_some()
        || declaration.child_by_field_name("interfaces").is_some()
    {
        return true;
    }
    let mut cursor = declaration.walk();
    declaration
        .named_children(&mut cursor)
        .any(|child| child.kind() == "extends_interfaces")
}

/// The variable an assignment's left side names: `name` or `this.name`.
fn assigned_name<'a>(left: Node<'_>, text: &impl Fn(Node<'_>) -> &'a str) -> Option<&'a str> {
    match left.kind() {
        "identifier" => Some(text(left)),
        "field_access" => {
            let object = left.child_by_field_name("object")?;
            let field = left.child_by_field_name("field")?;
            (object.kind() == "this").then(|| text(field))
        }
        _ => None,
    }
}

/// A string literal's content, not a text block. The simple escapes (`\\`, `\"`, `\'`,
/// `\b`, `\t`, `\n`, `\f`, `\r`, `\s`) are decoded; an octal or unicode escape makes it
/// unreadable.
fn literal_text<'a>(node: Node<'_>, source: &'a str) -> Option<Cow<'a, str>> {
    if node.kind() != "string_literal" {
        return None;
    }
    let text = source.get(node.byte_range())?;
    if text.starts_with("\"\"\"") {
        return None;
    }
    let mut cursor = node.walk();
    if node
        .named_children(&mut cursor)
        .any(|child| !matches!(child.kind(), "string_fragment" | "escape_sequence"))
    {
        return None;
    }
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    if inner.contains(UNKNOWN) {
        return None;
    }
    if !inner.contains('\\') {
        return Some(Cow::Borrowed(inner));
    }
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        out.push(match chars.next()? {
            '\\' => '\\',
            '"' => '"',
            '\'' => '\'',
            'b' => '\u{8}',
            't' => '\t',
            'n' => '\n',
            'f' => '\u{c}',
            'r' => '\r',
            's' => ' ',
            _ => return None,
        });
    }
    Some(Cow::Owned(out))
}

/// Looks through parentheses.
pub(super) fn unparenthesize(mut node: Node<'_>) -> Node<'_> {
    while node.kind() == "parenthesized_expression" {
        let mut cursor = node.walk();
        let Some(inner) = node.named_children(&mut cursor).find(|n| !comment(*n)) else {
            break;
        };
        node = inner;
    }
    node
}
