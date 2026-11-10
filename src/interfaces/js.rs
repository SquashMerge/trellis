//! A bounded static reader of JavaScript and TypeScript values for interface extraction
//! (#855): lexical bindings, string constants, imports and `require`, module exports, and the
//! framework and client objects that route registrations and client calls start from. Nothing
//! is inferred beyond what is written; anything else reads as `Unknown`.
use super::{charge, comment, parse};
use crate::references::{DocumentInput, Snapshot, work::Work};
use anyhow::Result;
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use tree_sitter::{Node, Tree};

/// How deep one value is followed through bindings, members and modules.
const EVAL_DEPTH: usize = 24;
/// How far a lexical lookup climbs.
const SCOPE_DEPTH: usize = 512;

pub(super) const VERBS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];

/// The written method name as an HTTP method, after uppercasing.
pub(super) fn verb(text: &str) -> Option<&'static str> {
    let upper = text.to_ascii_uppercase();
    VERBS.iter().copied().find(|verb| *verb == upper)
}

/// The request-local budget: the interface work bound and the caller's abort check.
pub(super) struct Budget<'b> {
    pub(super) work: &'b mut Work,
    pub(super) check: &'b mut dyn FnMut() -> Result<()>,
}

impl Budget<'_> {
    pub(super) fn step(&mut self, amount: usize) -> Result<()> {
        (self.check)()?;
        charge(self.work, amount)
    }
}

/// A framework or client object a call creates.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Factory {
    ExpressApp,
    ExpressRouter,
    FastifyApp,
    KoaApp,
    /// `new Router({prefix})`. `None` is a prefix that cannot be read.
    KoaRouter(Option<String>),
    /// `axios.create({baseURL})`.
    Axios(Base),
}

/// A client's base URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Base {
    None,
    Known(String),
    Unreadable,
}

/// Where a name is bound: its document and the start of its binding identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct BindingId {
    pub(super) doc: usize,
    pub(super) at: usize,
}

#[derive(Clone, Debug)]
pub(super) enum Value<'w> {
    Unknown,
    /// A string, with `{}` for each part that cannot be read.
    Str(String),
    /// A bare package specifier and the member path read from it.
    Package(String, Vec<String>),
    /// A JavaScript or TypeScript document, as a module.
    Module(usize),
    /// A function: its document, its node, and the declaration that names it, if any.
    Function {
        doc: usize,
        node: Node<'w>,
        declaration: Option<Node<'w>>,
    },
    /// An object literal.
    Object(usize, Node<'w>),
    /// A framework or client object, and the binding that holds it, if any.
    Made {
        factory: Factory,
        binding: Option<Binding<'w>>,
    },
    /// A function parameter.
    Param(Binding<'w>),
    /// `x.routes()` or `x.middleware()` of a value.
    Routes(Box<Value<'w>>),
    /// A method of an axios client.
    AxiosMethod(Base, String),
    /// A `fastify-plugin` plugin, which shares its parent's scope.
    Shared(Box<Value<'w>>),
}

/// A binding: its identity, its name, and the node whose span is its scope.
#[derive(Clone, Copy, Debug)]
pub(super) struct Binding<'w> {
    pub(super) id: BindingId,
    /// The binding identifier.
    pub(super) name: Node<'w>,
    pub(super) scope: Node<'w>,
}

enum Found<'w> {
    Param(Binding<'w>),
    Declarator(Binding<'w>, Node<'w>),
    /// A destructuring declarator: the binding, the declarator and the member key.
    Destructured(Binding<'w>, Node<'w>, String),
    Function(Node<'w>),
    Import {
        source: String,
        name: Imported,
    },
}

#[derive(Clone)]
enum Imported {
    Default,
    Namespace,
    Named(String),
}

/// The parsed JavaScript and TypeScript documents of a snapshot, parsed on first use.
pub(super) struct World<'s> {
    pub(super) snapshot: &'s Snapshot,
    by_path: BTreeMap<&'s str, usize>,
    trees: Vec<OnceCell<Tree>>,
    /// Each scope's declaring statements by name, keyed by document and scope span.
    scopes: RefCell<HashMap<(usize, usize, usize), Rc<ScopeIndex>>>,
}

type ScopeIndex = HashMap<String, Vec<(usize, usize)>>;

/// The direct child of `scope` with exactly this span.
fn child_at(scope: Node<'_>, start: usize, end: usize) -> Option<Node<'_>> {
    let mut node = scope.descendant_for_byte_range(start, end)?;
    loop {
        let parent = node.parent()?;
        if parent.id() == scope.id() {
            return (node.start_byte() == start && node.end_byte() == end).then_some(node);
        }
        node = parent;
    }
}

pub(super) fn javascript(doc: &DocumentInput) -> bool {
    matches!(doc.key.language.as_str(), "javascript" | "typescript")
}

impl<'s> World<'s> {
    pub(super) fn new(snapshot: &'s Snapshot) -> Self {
        let by_path = snapshot
            .documents
            .iter()
            .enumerate()
            .filter(|(_, doc)| javascript(doc) && !snapshot.evidence_gaps.contains_key(&doc.key))
            .map(|(at, doc)| (doc.key.path.as_str(), at))
            .collect();
        Self {
            snapshot,
            by_path,
            trees: (0..snapshot.documents.len())
                .map(|_| OnceCell::new())
                .collect(),
            scopes: RefCell::default(),
        }
    }

    pub(super) fn doc(&self, at: usize) -> &'s DocumentInput {
        &self.snapshot.documents[at]
    }

    /// Whether the document is a JavaScript or TypeScript document with evidence.
    pub(super) fn readable(&self, at: usize) -> bool {
        self.by_path.get(self.doc(at).key.path.as_str()) == Some(&at)
    }

    /// The document's parse tree, charging its length once.
    pub(super) fn tree(&self, at: usize, budget: &mut Budget<'_>) -> Result<Option<&Tree>> {
        if !self.readable(at) {
            return Ok(None);
        }
        if let Some(tree) = self.trees[at].get() {
            return Ok(Some(tree));
        }
        let doc = self.doc(at);
        budget.step(doc.file.text.len())?;
        let language = match doc.key.language.as_str() {
            "typescript" => crate::indexer_ts::grammar(&doc.key.path),
            _ => tree_sitter_javascript::LANGUAGE.into(),
        };
        let tree = parse(doc, &language, &mut budget.check)?;
        let _ = self.trees[at].set(tree);
        Ok(self.trees[at].get())
    }

    pub(super) fn text(&self, doc: usize, node: Node<'_>) -> &'s str {
        self.doc(doc).file.text.get(node.byte_range()).unwrap_or("")
    }

    /// The document a relative specifier names, trying the usual extensions and `index`.
    fn resolve_module(&self, from: usize, specifier: &str) -> Option<usize> {
        if !(specifier.starts_with("./") || specifier.starts_with("../")) {
            return None;
        }
        let from = &self.doc(from).key.path;
        let mut parts: Vec<&str> = from.split('/').collect();
        parts.pop();
        for part in specifier.split('/') {
            match part {
                "." | "" => {}
                ".." => {
                    parts.pop()?;
                }
                part => parts.push(part),
            }
        }
        let base = parts.join("/");
        const EXTENSIONS: [&str; 8] = ["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];
        let stem = base
            .strip_suffix(".js")
            .or_else(|| base.strip_suffix(".mjs"))
            .or_else(|| base.strip_suffix(".cjs"));
        std::iter::once(base.clone())
            .chain(EXTENSIONS.iter().map(|ext| format!("{base}.{ext}")))
            .chain(
                stem.into_iter().flat_map(|stem| {
                    ["ts", "tsx", "mts", "cts"].map(|ext| format!("{stem}.{ext}"))
                }),
            )
            .chain(EXTENSIONS.iter().map(|ext| format!("{base}/index.{ext}")))
            .find_map(|path| self.by_path.get(path.as_str()).copied())
    }
}

/// Removes parentheses, `await` and TypeScript assertions around an expression.
pub(super) fn unwrap(mut node: Node<'_>) -> Node<'_> {
    loop {
        let inner = match node.kind() {
            "parenthesized_expression" | "await_expression" | "non_null_expression" => {
                first_named(node)
            }
            "as_expression" | "satisfies_expression" => node.named_child(0),
            "type_assertion" => node.named_child(1),
            _ => None,
        };
        match inner {
            Some(inner) => node = inner,
            None => return node,
        }
    }
}

fn first_named(node: Node<'_>) -> Option<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).find(|n| !comment(*n))
}

/// The named children of a node, without comments.
pub(super) fn children(node: Node<'_>) -> Vec<Node<'_>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .filter(|child| !comment(*child))
        .collect()
}

/// A call's arguments, without comments.
pub(super) fn arguments(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments")
        .map(children)
        .unwrap_or_default()
}

pub(super) fn function_like(node: Node<'_>) -> bool {
    matches!(
        node.kind(),
        "function_declaration"
            | "function_expression"
            | "function"
            | "arrow_function"
            | "method_definition"
            | "generator_function_declaration"
            | "generator_function"
    )
}

/// A function's parameter names, in order; `None` for a pattern parameter.
fn parameters<'w>(function: Node<'w>) -> Vec<Option<Node<'w>>> {
    if let Some(single) = function.child_by_field_name("parameter") {
        return vec![(single.kind() == "identifier").then_some(single)];
    }
    let Some(list) = function.child_by_field_name("parameters") else {
        return Vec::new();
    };
    children(list)
        .into_iter()
        .map(|parameter| {
            let parameter = match parameter.kind() {
                "required_parameter" | "optional_parameter" => {
                    parameter.child_by_field_name("pattern")?
                }
                _ => parameter,
            };
            let parameter = match parameter.kind() {
                "assignment_pattern" => parameter.child_by_field_name("left")?,
                _ => parameter,
            };
            (parameter.kind() == "identifier").then_some(parameter)
        })
        .collect()
}

impl<'s> World<'s> {
    /// The binding a name has at a node: the innermost parameter or declaration in scope.
    fn lookup<'w>(
        &'w self,
        doc: usize,
        name: &str,
        at: Node<'w>,
        budget: &mut Budget<'_>,
    ) -> Result<Option<Found<'w>>> {
        let mut node = at;
        for _ in 0..SCOPE_DEPTH {
            let Some(parent) = node.parent() else {
                return Ok(None);
            };
            node = parent;
            budget.step(1)?;
            if function_like(node) {
                for parameter in parameters(node) {
                    if let Some(parameter) = parameter.filter(|p| self.text(doc, *p) == name) {
                        return Ok(Some(Found::Param(Binding {
                            id: BindingId {
                                doc,
                                at: parameter.start_byte(),
                            },
                            name: parameter,
                            scope: node,
                        })));
                    }
                }
                continue;
            }
            if !matches!(
                node.kind(),
                "program" | "statement_block" | "switch_case" | "switch_default"
            ) {
                continue;
            }
            let index = self.scope_index(doc, node, budget)?;
            for (start, end) in index.get(name).into_iter().flatten() {
                let Some(statement) = child_at(node, *start, *end) else {
                    continue;
                };
                if let Some(found) = self.declares(doc, statement, name, node) {
                    return Ok(Some(found));
                }
            }
        }
        Ok(None)
    }

    /// The spans of a scope's statements that declare each name, built once per scope.
    fn scope_index(
        &self,
        doc: usize,
        scope: Node<'_>,
        budget: &mut Budget<'_>,
    ) -> Result<Rc<ScopeIndex>> {
        let key = (doc, scope.start_byte(), scope.end_byte());
        if let Some(index) = self.scopes.borrow().get(&key) {
            return Ok(Rc::clone(index));
        }
        let statements = children(scope);
        budget.step(statements.len())?;
        let mut index = ScopeIndex::new();
        for statement in statements {
            for name in self.declared_names(doc, statement) {
                index
                    .entry(name.to_owned())
                    .or_default()
                    .push((statement.start_byte(), statement.end_byte()));
            }
        }
        let index = Rc::new(index);
        self.scopes.borrow_mut().insert(key, Rc::clone(&index));
        Ok(index)
    }

    /// Every name a statement declares, as [`Self::declares`] reads them.
    fn declared_names(&self, doc: usize, statement: Node<'_>) -> Vec<&'s str> {
        let statement = match statement.kind() {
            "export_statement" => match statement.child_by_field_name("declaration") {
                Some(declaration) => declaration,
                None => return Vec::new(),
            },
            _ => statement,
        };
        let mut names = Vec::new();
        match statement.kind() {
            "lexical_declaration" | "variable_declaration" => {
                for declarator in children(statement) {
                    let Some(target) = declarator.child_by_field_name("name") else {
                        continue;
                    };
                    match target.kind() {
                        "identifier" => names.push(self.text(doc, target)),
                        "object_pattern" => {
                            for property in children(target) {
                                match property.kind() {
                                    "shorthand_property_identifier_pattern" => {
                                        names.push(self.text(doc, property));
                                    }
                                    "pair_pattern" => {
                                        if let Some(value) = property
                                            .child_by_field_name("value")
                                            .filter(|v| v.kind() == "identifier")
                                        {
                                            names.push(self.text(doc, value));
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            "function_declaration" | "generator_function_declaration" => {
                if let Some(name) = statement.child_by_field_name("name") {
                    names.push(self.text(doc, name));
                }
            }
            "import_statement" => {
                for clause in children(statement) {
                    if clause.kind() != "import_clause" {
                        continue;
                    }
                    for part in children(clause) {
                        match part.kind() {
                            "identifier" => names.push(self.text(doc, part)),
                            "namespace_import" => {
                                names.extend(children(part).first().map(|id| self.text(doc, *id)));
                            }
                            "named_imports" => {
                                for specifier in children(part) {
                                    if let Some(local) = specifier
                                        .child_by_field_name("alias")
                                        .or_else(|| specifier.child_by_field_name("name"))
                                    {
                                        names.push(self.text(doc, local));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
        names
    }

    /// Whether a statement of a scope declares the name.
    fn declares<'w>(
        &'w self,
        doc: usize,
        statement: Node<'w>,
        name: &str,
        scope: Node<'w>,
    ) -> Option<Found<'w>> {
        let statement = match statement.kind() {
            "export_statement" => statement.child_by_field_name("declaration")?,
            _ => statement,
        };
        match statement.kind() {
            "lexical_declaration" | "variable_declaration" => {
                for declarator in children(statement) {
                    if declarator.kind() != "variable_declarator" {
                        continue;
                    }
                    let target = declarator.child_by_field_name("name")?;
                    match target.kind() {
                        "identifier" if self.text(doc, target) == name => {
                            return Some(Found::Declarator(
                                Binding {
                                    id: BindingId {
                                        doc,
                                        at: target.start_byte(),
                                    },
                                    name: target,
                                    scope,
                                },
                                declarator,
                            ));
                        }
                        "object_pattern" => {
                            for property in children(target) {
                                let (key, value) = match property.kind() {
                                    "shorthand_property_identifier_pattern" => {
                                        (self.text(doc, property), property)
                                    }
                                    "pair_pattern" => {
                                        let key = property.child_by_field_name("key")?;
                                        let value = property.child_by_field_name("value")?;
                                        if value.kind() != "identifier" {
                                            continue;
                                        }
                                        (self.text(doc, key), value)
                                    }
                                    _ => continue,
                                };
                                if self.text(doc, value) == name {
                                    return Some(Found::Destructured(
                                        Binding {
                                            id: BindingId {
                                                doc,
                                                at: value.start_byte(),
                                            },
                                            name: value,
                                            scope,
                                        },
                                        declarator,
                                        key.to_owned(),
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
                None
            }
            "function_declaration" | "generator_function_declaration" => {
                let own = statement.child_by_field_name("name")?;
                (self.text(doc, own) == name).then_some(Found::Function(statement))
            }
            "import_statement" => {
                let source = self.string(doc, statement.child_by_field_name("source")?)?;
                let clause = children(statement)
                    .into_iter()
                    .find(|child| child.kind() == "import_clause")?;
                for part in children(clause) {
                    match part.kind() {
                        "identifier" if self.text(doc, part) == name => {
                            return Some(Found::Import {
                                source,
                                name: Imported::Default,
                            });
                        }
                        "namespace_import"
                            if children(part)
                                .first()
                                .is_some_and(|id| self.text(doc, *id) == name) =>
                        {
                            return Some(Found::Import {
                                source,
                                name: Imported::Namespace,
                            });
                        }
                        "named_imports" => {
                            for specifier in children(part) {
                                let imported = specifier.child_by_field_name("name")?;
                                let local =
                                    specifier.child_by_field_name("alias").unwrap_or(imported);
                                if self.text(doc, local) == name {
                                    let imported = match imported.kind() {
                                        "string" => self.string(doc, imported)?,
                                        _ => self.text(doc, imported).to_owned(),
                                    };
                                    return Some(Found::Import {
                                        source,
                                        name: if imported == "default" {
                                            Imported::Default
                                        } else {
                                            Imported::Named(imported)
                                        },
                                    });
                                }
                            }
                        }
                        _ => {}
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// A string literal's value, with no escapes and no substitutions.
    pub(super) fn string(&self, doc: usize, node: Node<'_>) -> Option<String> {
        if !matches!(node.kind(), "string" | "template_string") {
            return None;
        }
        let mut text = String::new();
        for child in children(node) {
            match child.kind() {
                "string_fragment" => text.push_str(self.text(doc, child)),
                _ => return None,
            }
        }
        Some(text)
    }

    /// The value of an expression.
    pub(super) fn eval<'w>(
        &'w self,
        doc: usize,
        node: Node<'w>,
        budget: &mut Budget<'_>,
    ) -> Result<Value<'w>> {
        self.eval_at(doc, node, budget, 0)
    }

    fn eval_at<'w>(
        &'w self,
        doc: usize,
        node: Node<'w>,
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Value<'w>> {
        budget.step(1)?;
        if depth > EVAL_DEPTH || node.has_error() {
            return Ok(Value::Unknown);
        }
        let node = unwrap(node);
        let next = depth + 1;
        Ok(match node.kind() {
            "string" => self.string(doc, node).map_or(Value::Unknown, Value::Str),
            "template_string" => {
                let mut text = String::new();
                for child in children(node) {
                    match child.kind() {
                        "string_fragment" => text.push_str(self.text(doc, child)),
                        "template_substitution" => {
                            let value = match first_named(child) {
                                Some(inner) => self.eval_at(doc, inner, budget, next)?,
                                None => Value::Unknown,
                            };
                            match value {
                                Value::Str(part) => text.push_str(&part),
                                _ => text.push_str("{}"),
                            }
                        }
                        _ => return Ok(Value::Unknown),
                    }
                }
                Value::Str(text)
            }
            "binary_expression" => {
                let operator = node
                    .child_by_field_name("operator")
                    .map_or("", |op| self.text(doc, op));
                let (Some(left), Some(right)) = (
                    node.child_by_field_name("left"),
                    node.child_by_field_name("right"),
                ) else {
                    return Ok(Value::Unknown);
                };
                if operator != "+" {
                    return Ok(Value::Unknown);
                }
                let left = self.eval_at(doc, left, budget, next)?;
                let right = self.eval_at(doc, right, budget, next)?;
                match (left, right) {
                    (Value::Str(a), Value::Str(b)) => Value::Str(a + &b),
                    (Value::Str(a), _) => Value::Str(a + "{}"),
                    (_, Value::Str(b)) => Value::Str(format!("{{}}{b}")),
                    _ => Value::Unknown,
                }
            }
            "identifier" => {
                let name = self.text(doc, node);
                match self.lookup(doc, name, node, budget)? {
                    None => Value::Unknown,
                    Some(found) => self.found(doc, found, budget, next)?,
                }
            }
            "member_expression" => {
                let (Some(object), Some(property)) = (
                    node.child_by_field_name("object"),
                    node.child_by_field_name("property"),
                ) else {
                    return Ok(Value::Unknown);
                };
                let property = self.text(doc, property).to_owned();
                let object = self.eval_at(doc, object, budget, next)?;
                self.member(object, &property, budget, next)?
            }
            "call_expression" => self.call(doc, node, budget, next)?,
            "new_expression" => {
                let Some(constructor) = node.child_by_field_name("constructor") else {
                    return Ok(Value::Unknown);
                };
                let arguments = arguments(node);
                match self.eval_at(doc, constructor, budget, next)? {
                    Value::Package(package, path) => {
                        self.construct(doc, &package, &path, &arguments, budget, next)?
                    }
                    _ => Value::Unknown,
                }
            }
            "function_expression" | "function" | "arrow_function" | "generator_function" => {
                Value::Function {
                    doc,
                    node,
                    declaration: None,
                }
            }
            "object" => Value::Object(doc, node),
            _ => Value::Unknown,
        })
    }

    fn found<'w>(
        &'w self,
        doc: usize,
        found: Found<'w>,
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Value<'w>> {
        Ok(match found {
            Found::Param(binding) => Value::Param(binding),
            Found::Function(node) => Value::Function {
                doc,
                node,
                declaration: Some(node),
            },
            Found::Declarator(binding, declarator) => {
                let Some(value) = declarator.child_by_field_name("value") else {
                    return Ok(Value::Unknown);
                };
                // Only a `const` binding keeps its initial value.
                let constant = declarator
                    .parent()
                    .and_then(|declaration| declaration.child(0))
                    .is_some_and(|keyword| keyword.kind() == "const");
                match self.eval_at(doc, value, budget, depth)? {
                    Value::Function { doc, node, .. } => Value::Function {
                        doc,
                        node,
                        declaration: Some(declarator),
                    },
                    Value::Made {
                        factory,
                        binding: None,
                    } => Value::Made {
                        factory,
                        binding: Some(binding),
                    },
                    Value::Str(_) | Value::Object(..) if !constant => Value::Unknown,
                    other => other,
                }
            }
            Found::Destructured(_, declarator, key) => {
                let Some(value) = declarator.child_by_field_name("value") else {
                    return Ok(Value::Unknown);
                };
                let value = self.eval_at(doc, value, budget, depth)?;
                self.member(value, &key, budget, depth)?
            }
            Found::Import { source, name } => match self.resolve_module(doc, &source) {
                Some(module) => match name {
                    Imported::Namespace => Value::Module(module),
                    Imported::Default => self.export(module, None, budget, depth)?,
                    Imported::Named(name) => self.export(module, Some(&name), budget, depth)?,
                },
                None if source.starts_with('.') => Value::Unknown,
                None => match name {
                    Imported::Namespace | Imported::Default => Value::Package(source, vec![]),
                    Imported::Named(name) => Value::Package(source, vec![name]),
                },
            },
        })
    }

    fn member<'w>(
        &'w self,
        object: Value<'w>,
        property: &str,
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Value<'w>> {
        Ok(match object {
            Value::Package(package, mut path) => {
                // `import * as x` and `require` interop: `x.default` is the package itself.
                if !(path.is_empty() && property == "default") {
                    path.push(property.to_owned());
                }
                Value::Package(package, path)
            }
            Value::Module(module) if property == "default" => {
                self.export(module, None, budget, depth)?
            }
            Value::Module(module) => self.export(module, Some(property), budget, depth)?,
            Value::Made {
                factory: Factory::Axios(base),
                ..
            } => Value::AxiosMethod(base, property.to_owned()),
            Value::Object(doc, object) => match self.property(doc, object, property) {
                Prop::Found(value) => self.eval_at(doc, value, budget, depth)?,
                Prop::Absent | Prop::Unknown => Value::Unknown,
            },
            _ => Value::Unknown,
        })
    }

    /// What an object literal says about `key`, in write order: the last member that may set
    /// it decides. A spread, a computed key, or an accessor or method of that name may set it
    /// to anything, so it is `Unknown` until a later explicit `key` member.
    pub(super) fn property<'w>(&'w self, doc: usize, object: Node<'w>, key: &str) -> Prop<'w> {
        let mut found = Prop::Absent;
        for member in children(object) {
            match member.kind() {
                "pair" => match (
                    member.child_by_field_name("key"),
                    member.child_by_field_name("value"),
                ) {
                    (Some(name), _) if name.kind() == "computed_property_name" => {
                        found = Prop::Unknown;
                    }
                    (Some(name), Some(value))
                        if self.key_name(doc, name).as_deref() == Some(key) =>
                    {
                        found = Prop::Found(value);
                    }
                    _ => {}
                },
                "shorthand_property_identifier" if self.text(doc, member) == key => {
                    found = Prop::Found(member);
                }
                "spread_element" => found = Prop::Unknown,
                "method_definition" => match member.child_by_field_name("name") {
                    Some(name)
                        if name.kind() == "computed_property_name"
                            || self.key_name(doc, name).as_deref() == Some(key) =>
                    {
                        found = Prop::Unknown;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
        found
    }

    /// An object member's written name: an identifier, or a string literal without escapes.
    pub(super) fn key_name(&self, doc: usize, key: Node<'_>) -> Option<String> {
        match key.kind() {
            "property_identifier" => Some(self.text(doc, key).to_owned()),
            _ => self.string(doc, key),
        }
    }

    fn call<'w>(
        &'w self,
        doc: usize,
        node: Node<'w>,
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Value<'w>> {
        let Some(function) = node.child_by_field_name("function") else {
            return Ok(Value::Unknown);
        };
        let arguments = arguments(node);
        let function = unwrap(function);
        if function.kind() == "identifier"
            && self.text(doc, function) == "require"
            && let [specifier] = arguments[..]
            && let Some(source) = self.string(doc, specifier)
        {
            return Ok(match self.resolve_module(doc, &source) {
                Some(module) => self.whole(module, budget, depth)?,
                None if source.starts_with('.') => Value::Unknown,
                None => Value::Package(source, vec![]),
            });
        }
        if function.kind() == "member_expression"
            && let Some(property) = function.child_by_field_name("property")
            && matches!(self.text(doc, property), "routes" | "middleware")
            && let Some(object) = function.child_by_field_name("object")
        {
            return Ok(Value::Routes(Box::new(
                self.eval_at(doc, object, budget, depth)?,
            )));
        }
        Ok(match self.eval_at(doc, function, budget, depth)? {
            Value::Package(package, path) => {
                let path: Vec<&str> = path.iter().map(String::as_str).collect();
                match (package.as_str(), &path[..]) {
                    ("express", []) => made(Factory::ExpressApp),
                    ("express", ["Router"]) => made(Factory::ExpressRouter),
                    ("fastify", [] | ["fastify"]) => made(Factory::FastifyApp),
                    ("axios", ["create"]) => made(Factory::Axios(self.base(
                        doc,
                        arguments.first().copied(),
                        budget,
                        depth,
                    )?)),
                    ("fastify-plugin", []) => {
                        self.fastify_plugin(doc, &arguments, budget, depth)?
                    }
                    _ => Value::Unknown,
                }
            }
            _ => Value::Unknown,
        })
    }

    /// `fp(plugin, options)` from `fastify-plugin`. Such a plugin is not encapsulated: it
    /// shares its parent's scope, so `register`'s `prefix` does not apply to its routes. The
    /// Fastify reference says so under "Route Prefixing and fastify-plugin":
    /// <https://fastify.dev/docs/latest/Reference/Routes/>. With `encapsulate: true` it is an
    /// ordinary plugin. Options that cannot be read decide nothing, so the value is unknown.
    fn fastify_plugin<'w>(
        &'w self,
        doc: usize,
        arguments: &[Node<'w>],
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Value<'w>> {
        let Some(plugin) = arguments.first() else {
            return Ok(Value::Unknown);
        };
        let encapsulated = match arguments.get(1) {
            None => false,
            Some(options) => match self.eval_at(doc, *options, budget, depth)? {
                Value::Object(at, object) => match self.property(at, object, "encapsulate") {
                    Prop::Absent => false,
                    Prop::Found(value) => match unwrap(value).kind() {
                        "true" => true,
                        "false" => false,
                        _ => return Ok(Value::Unknown),
                    },
                    Prop::Unknown => return Ok(Value::Unknown),
                },
                _ => return Ok(Value::Unknown),
            },
        };
        let plugin = self.eval_at(doc, *plugin, budget, depth)?;
        Ok(if encapsulated {
            plugin
        } else {
            Value::Shared(Box::new(plugin))
        })
    }

    fn construct<'w>(
        &'w self,
        doc: usize,
        package: &str,
        path: &[String],
        arguments: &[Node<'w>],
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Value<'w>> {
        Ok(match (package, path) {
            ("koa", []) => made(Factory::KoaApp),
            ("@koa/router" | "koa-router", []) => {
                let prefix = match arguments.first() {
                    None => Some(String::new()),
                    Some(options) => match self.eval_at(doc, *options, budget, depth)? {
                        Value::Object(doc, object) => match self.property(doc, object, "prefix") {
                            Prop::Absent => Some(String::new()),
                            Prop::Unknown => None,
                            Prop::Found(prefix) => {
                                match self.eval_at(doc, prefix, budget, depth)? {
                                    Value::Str(prefix) if readable_path(&prefix) => Some(prefix),
                                    _ => None,
                                }
                            }
                        },
                        _ => None,
                    },
                };
                made(Factory::KoaRouter(prefix))
            }
            _ => Value::Unknown,
        })
    }

    /// The `baseURL` of an axios config.
    pub(super) fn base(
        &self,
        doc: usize,
        config: Option<Node<'_>>,
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Base> {
        let Some(config) = config else {
            return Ok(Base::None);
        };
        Ok(match self.eval_at(doc, config, budget, depth)? {
            Value::Object(doc, object) => match self.property(doc, object, "baseURL") {
                Prop::Unknown => Base::Unreadable,
                Prop::Absent => Base::None,
                Prop::Found(value) => match self.eval_at(doc, value, budget, depth)? {
                    Value::Str(base) => Base::Known(base),
                    _ => Base::Unreadable,
                },
            },
            _ => Base::Unreadable,
        })
    }

    /// What `require` of a module gives: `module.exports`, else the module itself.
    fn whole<'w>(
        &'w self,
        module: usize,
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Value<'w>> {
        let Some(tree) = self.tree(module, budget)? else {
            return Ok(Value::Unknown);
        };
        let statements = children(tree.root_node());
        budget.step(statements.len())?;
        let mut found = None;
        for statement in statements {
            if let Some((None, value)) = self.commonjs(module, statement) {
                found = Some(value);
            }
        }
        match found {
            Some(value) => self.eval_at(module, value, budget, depth + 1),
            None => Ok(Value::Module(module)),
        }
    }

    /// `module.exports = v` as `(None, v)`, and `exports.n = v` or `module.exports.n = v` as
    /// `(Some(n), v)`.
    fn commonjs<'w>(&self, doc: usize, statement: Node<'w>) -> Option<(Option<&'s str>, Node<'w>)> {
        if statement.kind() != "expression_statement" {
            return None;
        }
        let assignment = first_named(statement)?;
        if assignment.kind() != "assignment_expression" {
            return None;
        }
        let left = assignment.child_by_field_name("left")?;
        let right = assignment.child_by_field_name("right")?;
        let target = self.text(doc, left);
        let target: String = target.split_whitespace().collect();
        if target == "module.exports" {
            return Some((None, right));
        }
        let name = target
            .strip_prefix("module.exports.")
            .or_else(|| target.strip_prefix("exports."))?;
        let property = left.child_by_field_name("property")?;
        (name == self.text(doc, property)).then_some((Some(self.text(doc, property)), right))
    }

    /// A module's export: `None` is the default export.
    fn export<'w>(
        &'w self,
        module: usize,
        name: Option<&str>,
        budget: &mut Budget<'_>,
        depth: usize,
    ) -> Result<Value<'w>> {
        let Some(tree) = self.tree(module, budget)? else {
            return Ok(Value::Unknown);
        };
        let root = tree.root_node();
        let statements = children(root);
        budget.step(statements.len())?;
        let mut whole = None;
        for statement in statements {
            if let Some((exported, value)) = self.commonjs(module, statement) {
                match exported {
                    None => whole = Some(value),
                    Some(exported) if Some(exported) == name => {
                        return self.eval_at(module, value, budget, depth + 1);
                    }
                    Some(_) => {}
                }
                continue;
            }
            if statement.kind() != "export_statement" {
                continue;
            }
            let default = (0..statement.child_count())
                .filter_map(|at| statement.child(at))
                .any(|child| child.kind() == "default");
            let declaration = statement.child_by_field_name("declaration");
            match name {
                None if default => {
                    if let Some(value) = statement.child_by_field_name("value") {
                        return self.eval_at(module, value, budget, depth + 1);
                    }
                    if let Some(declaration) = declaration
                        && function_like(declaration)
                    {
                        return Ok(Value::Function {
                            doc: module,
                            node: declaration,
                            declaration: declaration
                                .child_by_field_name("name")
                                .map(|_| declaration),
                        });
                    }
                }
                Some(name) if !default => {
                    if let Some(declaration) = declaration
                        && let Some(found) = self.declares(module, declaration, name, root)
                    {
                        return self.found(module, found, budget, depth + 1);
                    }
                }
                _ => {}
            }
            // `export { a, b as c }` and `export { a as default }`.
            for clause in children(statement) {
                if clause.kind() != "export_clause" {
                    continue;
                }
                for specifier in children(clause) {
                    let Some(local) = specifier.child_by_field_name("name") else {
                        continue;
                    };
                    let exported = specifier
                        .child_by_field_name("alias")
                        .map_or_else(|| self.text(module, local), |a| self.text(module, a));
                    let wanted = name.unwrap_or("default");
                    if exported == wanted && statement.child_by_field_name("source").is_none() {
                        return self.eval_at(module, local, budget, depth + 1);
                    }
                }
            }
        }
        // CommonJS interop: the default import is `module.exports`, and a named import is
        // one of its members.
        match (whole, name) {
            (Some(value), None) => self.eval_at(module, value, budget, depth + 1),
            (Some(value), Some(name)) => {
                let value = self.eval_at(module, value, budget, depth + 1)?;
                self.member(value, name, budget, depth + 1)
            }
            (None, _) => Ok(Value::Unknown),
        }
    }

    /// The variable or parameter binding an identifier has where it is written.
    pub(super) fn binding<'w>(
        &'w self,
        doc: usize,
        identifier: Node<'w>,
        budget: &mut Budget<'_>,
    ) -> Result<Option<Binding<'w>>> {
        if identifier.kind() != "identifier" {
            return Ok(None);
        }
        let name = self.text(doc, identifier);
        Ok(match self.lookup(doc, name, identifier, budget)? {
            Some(Found::Param(binding) | Found::Declarator(binding, _)) => Some(binding),
            Some(Found::Destructured(binding, ..)) => Some(binding),
            _ => None,
        })
    }

    /// The binding of a function's parameter at `index`, when it is a plain identifier.
    pub(super) fn parameter<'w>(
        &'w self,
        doc: usize,
        function: Node<'w>,
        index: usize,
    ) -> Option<Binding<'w>> {
        let name = parameters(function).into_iter().nth(index)??;
        Some(Binding {
            id: BindingId {
                doc,
                at: name.start_byte(),
            },
            name,
            scope: function,
        })
    }
}

fn made<'w>(factory: Factory) -> Value<'w> {
    Value::Made {
        factory,
        binding: None,
    }
}

/// What an object literal says about one key.
#[derive(Clone, Copy, Debug)]
pub(super) enum Prop<'w> {
    /// No member can set it.
    Absent,
    /// The last member that sets it, with this value.
    Found(Node<'w>),
    /// A spread, computed key, accessor or method may set it to anything.
    Unknown,
}

/// Whether a server path or prefix is a rooted literal this extraction can read: no
/// unreadable part and no pattern syntax.
pub(super) fn readable_path(path: &str) -> bool {
    (path.is_empty() || path.starts_with('/'))
        && !path.contains(['{', '}', '(', ')', '?', '+', '[', ']', '#', '|', '\\'])
}
