//! Server routes: axum literal registrations and the dispatch tables behind a catch-all, and
//! actix-web routes (#858).
mod actix;
pub(super) use actix::join;

use super::{
    Handler, HandlerKind, LineIndex, Route, charge, comment, named_callable, parse, segments,
    test_file, test_span, under_test_support,
};
use crate::native_evidence::{Declaration, Range};
use crate::references::{DocumentInput, Snapshot, walk, work::Work};
use anyhow::Result;
use std::cell::OnceCell;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

const VERBS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
const NON_ANSWERS: [&str; 4] = ["unreachable", "panic", "todo", "unimplemented"];
const ASSETS: [&str; 2] = ["include_str", "include_bytes"];

/// The HTTP method a router function such as `get` registers.
fn router_verb(name: &str) -> Option<&'static str> {
    VERBS
        .iter()
        .copied()
        .find(|verb| verb.eq_ignore_ascii_case(name) && name.chars().all(|c| c.is_lowercase()))
}

/// The HTTP method a `Method::GET` path names.
pub(super) fn path_verb(path: &str) -> Option<&'static str> {
    let mut parts = path.rsplit("::");
    let last = parts.next()?;
    let verb = VERBS.iter().copied().find(|verb| *verb == last)?;
    (parts.next() == Some("Method")).then_some(verb)
}

struct Found {
    path: String,
    start: usize,
    route: Route,
}

/// The server routes of every production Rust document, one per method and template.
pub(super) fn extract(
    snapshot: &Snapshot,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Route>> {
    let mut found = Vec::new();
    let mut actix = actix::Collected::default();
    for doc in &snapshot.documents {
        check()?;
        let text = &doc.file.text;
        let axum = text.contains(".route(") || text.contains("Method::");
        let actix_web = actix::candidate(text);
        if doc.key.language != "rust"
            || snapshot.evidence_gaps.contains_key(&doc.key)
            || !(axum || actix_web)
            || test_file(doc)
        {
            continue;
        }
        charge(work, text.len())?;
        let tree = parse(doc, &tree_sitter_rust::LANGUAGE.into(), check)?;
        let document = Document::new(doc, tree.root_node());
        if axum {
            document.collect(&mut found, check)?;
        }
        if actix_web {
            actix::collect(&document, &mut actix, &mut found, check)?;
        }
    }
    actix.finish(&mut found, work, check)?;
    found.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
    let mut seen = BTreeSet::new();
    Ok(found
        .into_iter()
        .filter(|f| seen.insert((f.route.method.clone(), f.route.path_template.clone())))
        .map(|f| f.route)
        .collect())
}

#[derive(Default)]
struct Scan<'a> {
    registrations: Vec<Node<'a>>,
    impls: Vec<Node<'a>>,
    arms: Vec<Node<'a>>,
}

/// One `match` arm of a dispatch table: `(&Method::GET, "git") => Some(Self::GitStatus)`.
struct Entry {
    method: &'static str,
    suffix: String,
    variant: String,
}

/// A `match` on `(method, path)` inside `impl T { fn f(..) }`.
struct Table {
    ty: String,
    function: String,
    span: Range,
    entries: Vec<Entry>,
}

/// A `match` arm anywhere in the document that may answer a dispatch table variant.
struct Arm<'a> {
    node: Node<'a>,
    patterns: Vec<String>,
    answers: bool,
    self_type: Option<&'a str>,
}

struct Document<'a> {
    doc: &'a DocumentInput,
    source: &'a str,
    root: Node<'a>,
    lines: LineIndex,
    keys: OnceCell<BTreeMap<String, String>>,
}

impl<'a> Document<'a> {
    fn new(doc: &'a DocumentInput, root: Node<'a>) -> Self {
        Self {
            doc,
            source: &doc.file.text,
            root,
            lines: LineIndex::new(&doc.file.text),
            keys: OnceCell::new(),
        }
    }

    fn text(&self, node: Node<'_>) -> &'a str {
        self.source.get(node.byte_range()).unwrap_or("")
    }

    fn range(node: Node<'_>) -> Range {
        Range {
            start: node.start_byte(),
            end: node.end_byte(),
        }
    }

    fn excluded(&self, node: Node<'_>) -> bool {
        test_span(self.doc, &Self::range(node)) || under_test_support(node, self.source)
    }

    fn collect(
        &self,
        found: &mut Vec<Found>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let scan = self.scan(check)?;
        let tables = self.tables(&scan, check)?;
        let arms = if tables.is_empty() {
            Vec::new()
        } else {
            self.arms(&scan, check)?
        };
        for call in &scan.registrations {
            check()?;
            for route in self.registration(*call, &tables, &arms, check)? {
                let literal = Self::arguments(*call)[0];
                found.push(Found {
                    path: self.doc.key.path.clone(),
                    start: literal.start_byte(),
                    route,
                });
            }
        }
        Ok(())
    }

    fn scan(&self, check: &mut impl FnMut() -> Result<()>) -> Result<Scan<'a>> {
        let mut scan = Scan::default();
        walk(self.root, check, &mut |node, _| {
            match node.kind() {
                "call_expression" if self.is_route_call(node) => scan.registrations.push(node),
                "impl_item" => scan.impls.push(node),
                "match_arm" => scan.arms.push(node),
                _ => {}
            }
            Ok(false)
        })?;
        Ok(scan)
    }

    fn is_route_call(&self, call: Node<'_>) -> bool {
        call.child_by_field_name("function")
            .filter(|function| function.kind() == "field_expression")
            .and_then(|function| function.child_by_field_name("field"))
            .is_some_and(|field| self.text(field) == "route")
    }

    fn named_children(node: Node<'a>) -> Vec<Node<'a>> {
        let mut cursor = node.walk();
        node.named_children(&mut cursor)
            .filter(|child| !comment(*child))
            .collect()
    }

    fn arguments(call: Node<'a>) -> Vec<Node<'a>> {
        call.child_by_field_name("arguments")
            .map(Self::named_children)
            .unwrap_or_default()
    }

    /// A string literal with no escapes.
    fn literal(&self, node: Node<'a>) -> Option<String> {
        if !matches!(node.kind(), "string_literal" | "raw_string_literal") {
            return None;
        }
        let mut text = String::new();
        for child in Self::named_children(node) {
            if child.kind() != "string_content" {
                return None;
            }
            text.push_str(self.text(child));
        }
        Some(text)
    }

    fn registration(
        &self,
        call: Node<'a>,
        tables: &[Table],
        arms: &[Arm<'a>],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Vec<Route>> {
        let arguments = Self::arguments(call);
        let [path, router] = arguments[..] else {
            return Ok(Vec::new());
        };
        let Some(literal) = self.literal(path).filter(|path| path.starts_with('/')) else {
            return Ok(Vec::new());
        };
        if self.excluded(call) {
            return Ok(Vec::new());
        }
        let template = normalize(&literal);
        let mut routes = Vec::new();
        for (method, handler) in self.method_pairs(router) {
            if let Some(expanded) =
                self.expand_table(&template, method, handler, tables, arms, check)?
            {
                routes.extend(expanded);
                continue;
            }
            if self.serves_asset(handler, check)? {
                continue;
            }
            let handler = match self.function(handler) {
                Some(function) => self.declaration_handler(function, None),
                None => self.span_handler(HandlerKind::Closure, Self::range(handler), None),
            };
            routes.push(Route {
                method: method.into(),
                segments: segments(&template),
                path_template: template.clone(),
                from_table: false,
                handler,
            });
        }
        Ok(routes)
    }

    /// `(method, handler)` pairs of `get(a).post(b)`, in written order.
    fn method_pairs(&self, router: Node<'a>) -> Vec<(&'static str, Node<'a>)> {
        let mut pairs = Vec::new();
        let mut node = router;
        while node.kind() == "call_expression" {
            let Some(function) = node.child_by_field_name("function") else {
                break;
            };
            let arguments = Self::arguments(node);
            let (name, chained) = match function.kind() {
                "identifier" => (function, None),
                "scoped_identifier" => match function.child_by_field_name("name") {
                    Some(name) => (name, None),
                    None => break,
                },
                "field_expression" => match function.child_by_field_name("field") {
                    Some(field) => (field, function.child_by_field_name("value")),
                    None => break,
                },
                _ => break,
            };
            if let (Some(method), [handler]) = (router_verb(self.text(name)), &arguments[..]) {
                pairs.push((method, *handler));
            }
            match chained {
                Some(next) => node = next,
                None => break,
            }
        }
        pairs.reverse();
        pairs
    }

    /// The only free function with this name in the document.
    fn function_named(&self, name: &str) -> Option<&'a Declaration> {
        let mut named = self
            .doc
            .declarations
            .iter()
            .filter(|d| d.kind == "function" && d.name.as_deref() == Some(name));
        match (named.next(), named.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    }

    /// The function an identifier names.
    fn function(&self, node: Node<'a>) -> Option<&'a Declaration> {
        (node.kind() == "identifier")
            .then(|| self.function_named(self.text(node)))
            .flatten()
    }

    fn function_node(&self, function: &Declaration) -> Option<Node<'a>> {
        let mut node = self
            .root
            .descendant_for_byte_range(function.range.start, function.range.end)?;
        while node.kind() != "function_item" {
            node = node.parent()?;
        }
        Some(node)
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

    fn declaration_handler(&self, function: &Declaration, variant: Option<&str>) -> Handler {
        Handler {
            kind: HandlerKind::Declaration,
            syntax_id: Some(function.syntax_id.clone()),
            stable_lookup_key: self.keys().get(&function.syntax_id).cloned(),
            document: self.doc.key.clone(),
            lines: self.lines.lines(&function.range),
            range: function.range.clone(),
            variant: variant.map(str::to_owned),
        }
    }

    /// A handler that is a span inside the innermost named callable around it.
    fn span_handler(&self, kind: HandlerKind, range: Range, variant: Option<&str>) -> Handler {
        let callable = named_callable(self.doc, &range);
        Handler {
            kind,
            syntax_id: callable.map(|d| d.syntax_id.clone()),
            stable_lookup_key: callable.and_then(|d| self.keys().get(&d.syntax_id).cloned()),
            document: self.doc.key.clone(),
            lines: self.lines.lines(&range),
            range,
            variant: variant.map(str::to_owned),
        }
    }

    /// Whether the node, or the function it names, embeds a static file.
    fn serves_asset(
        &self,
        handler: Node<'a>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        if self.embeds(handler, check)? {
            return Ok(true);
        }
        match self
            .function(handler)
            .and_then(|function| self.function_node(function))
        {
            Some(body) => self.embeds(body, check),
            None => Ok(false),
        }
    }

    fn embeds(&self, node: Node<'a>, check: &mut impl FnMut() -> Result<()>) -> Result<bool> {
        let mut found = false;
        walk(node, check, &mut |inner, _| {
            if inner.kind() == "macro_invocation" {
                let name = inner
                    .child_by_field_name("macro")
                    .map_or("", |name| self.text(name));
                found |= ASSETS.contains(&name.rsplit("::").next().unwrap_or(name).trim());
            }
            Ok(found)
        })?;
        Ok(found)
    }

    // Dispatch tables.

    fn tables(
        &self,
        scan: &Scan<'a>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Vec<Table>> {
        let mut tables = Vec::new();
        for implementation in &scan.impls {
            let Some(ty) = implementation
                .child_by_field_name("type")
                .filter(|ty| ty.kind() == "type_identifier")
                .map(|ty| self.text(ty))
            else {
                continue;
            };
            let Some(body) = implementation.child_by_field_name("body") else {
                continue;
            };
            for function in Self::named_children(body) {
                let Some(name) = function.child_by_field_name("name") else {
                    continue;
                };
                if function.kind() != "function_item" || self.excluded(function) {
                    continue;
                }
                let mut entries = Vec::new();
                let mut done = false;
                walk(function, check, &mut |node, check| {
                    if !done && node.kind() == "match_expression" {
                        self.entries(node, ty, false, &mut entries, check)?;
                        done = !entries.is_empty();
                    }
                    Ok(done)
                })?;
                if !entries.is_empty() {
                    tables.push(Table {
                        ty: ty.to_owned(),
                        function: self.text(name).to_owned(),
                        span: Self::range(function),
                        entries,
                    });
                }
            }
        }
        Ok(tables)
    }

    /// The `(&Method::M, "lit")` arms of a `match` on a 2-tuple. Arms of a nested table under
    /// the `_` arm answer `lit/{*}`.
    fn entries(
        &self,
        matched: Node<'a>,
        ty: &str,
        nested: bool,
        entries: &mut Vec<Entry>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let is_pair = matched.child_by_field_name("value").is_some_and(|value| {
            value.kind() == "tuple_expression" && Self::named_children(value).len() == 2
        });
        let Some(body) = matched.child_by_field_name("body").filter(|_| is_pair) else {
            return Ok(());
        };
        for arm in Self::named_children(body) {
            let (Some(pattern), Some(value)) = (
                arm.child_by_field_name("pattern"),
                arm.child_by_field_name("value"),
            ) else {
                continue;
            };
            let methods = self.method_patterns(pattern);
            if methods.is_empty() {
                if !nested && self.text(pattern).trim() == "_" {
                    walk(value, check, &mut |node, check| {
                        if node.kind() == "match_expression" {
                            self.entries(node, ty, true, entries, check)?;
                            return Ok(true);
                        }
                        Ok(false)
                    })?;
                }
                continue;
            }
            let Some(variant) = self.variant(value, ty, check)? else {
                continue;
            };
            for (method, literal) in methods {
                entries.push(Entry {
                    method,
                    suffix: if nested {
                        format!("{literal}/{{*}}")
                    } else {
                        literal
                    },
                    variant: variant.clone(),
                });
            }
        }
        Ok(())
    }

    /// `(&Method::GET, "git")` tuple patterns in a match pattern.
    fn method_patterns(&self, pattern: Node<'a>) -> Vec<(&'static str, String)> {
        let tuples = if pattern.kind() == "tuple_pattern" {
            vec![pattern]
        } else {
            Self::named_children(pattern)
                .into_iter()
                .filter(|child| child.kind() == "tuple_pattern")
                .collect()
        };
        tuples
            .into_iter()
            .filter_map(|tuple| {
                let [method, path] = Self::named_children(tuple)[..] else {
                    return None;
                };
                let method = path_verb(self.text(method).trim_start_matches('&').trim())?;
                Some((method, self.literal(path)?))
            })
            .collect()
    }

    /// The first `Self::V` or `T::V` in an arm.
    fn variant(
        &self,
        value: Node<'a>,
        ty: &str,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        let mut variant = None;
        walk(value, check, &mut |node, _| {
            if variant.is_none() && node.kind() == "scoped_identifier" {
                let path = node.child_by_field_name("path").map(|path| self.text(path));
                let name = node.child_by_field_name("name").map(|name| self.text(name));
                if let (Some(path), Some(name)) = (path, name)
                    && (path == "Self" || path == ty)
                    && name.starts_with(char::is_uppercase)
                {
                    variant = Some(name.to_owned());
                }
            }
            Ok(variant.is_some())
        })?;
        Ok(variant)
    }

    fn arms(
        &self,
        scan: &Scan<'a>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Vec<Arm<'a>>> {
        let mut arms = Vec::new();
        for node in &scan.arms {
            let mut patterns = Vec::new();
            if let Some(pattern) = node.child_by_field_name("pattern") {
                walk(pattern, check, &mut |inner, _| {
                    if inner.kind() == "scoped_identifier" {
                        patterns.push(self.text(inner).split_whitespace().collect());
                    }
                    Ok(false)
                })?;
            }
            let mut answers = true;
            if let Some(value) = node.child_by_field_name("value") {
                walk(value, check, &mut |inner, _| {
                    if inner.kind() == "macro_invocation" {
                        let name = inner
                            .child_by_field_name("macro")
                            .map_or("", |name| self.text(name));
                        answers &=
                            !NON_ANSWERS.contains(&name.rsplit("::").next().unwrap_or(name).trim());
                    }
                    Ok(false)
                })?;
            }
            let mut ancestor = node.parent();
            while let Some(parent) = ancestor.filter(|parent| parent.kind() != "impl_item") {
                ancestor = parent.parent();
            }
            arms.push(Arm {
                node: *node,
                patterns,
                answers,
                self_type: ancestor
                    .and_then(|implementation| implementation.child_by_field_name("type"))
                    .map(|ty| self.text(ty)),
            });
        }
        Ok(arms)
    }

    /// The routes a catch-all registration expands to, when its named handler calls a table.
    fn expand_table(
        &self,
        template: &str,
        method: &'static str,
        handler: Node<'a>,
        tables: &[Table],
        arms: &[Arm<'a>],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Vec<Route>>> {
        let last = template.rsplit('/').next().unwrap_or("");
        let Some(function) = self.function(handler).filter(|_| last.starts_with("{*")) else {
            return Ok(None);
        };
        let Some(body) = self.function_node(function) else {
            return Ok(None);
        };
        let mut table = None;
        for candidate in tables {
            if self.calls(body, candidate, check)? {
                table = Some(candidate);
                break;
            }
        }
        let Some(table) = table else {
            return Ok(None);
        };
        let prefix = &template[..template.len() - last.len()];
        let mut routes = Vec::new();
        for entry in table.entries.iter().filter(|entry| entry.method == method) {
            // A route whose answer serves a static asset is excluded, as a direct one is.
            let Some(handler) = self.dispatch_handler(table, entry, function, arms, check)? else {
                continue;
            };
            let path_template = format!("{prefix}{}", entry.suffix);
            routes.push(Route {
                method: method.into(),
                segments: segments(&path_template),
                path_template,
                from_table: true,
                handler,
            });
        }
        Ok(Some(routes))
    }

    /// Whether the function body calls `T::f(..)`.
    fn calls(
        &self,
        body: Node<'a>,
        table: &Table,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<bool> {
        let mut found = false;
        walk(body, check, &mut |node, _| {
            if node.kind() == "call_expression"
                && let Some(function) = node
                    .child_by_field_name("function")
                    .filter(|function| function.kind() == "scoped_identifier")
            {
                let at = |field| function.child_by_field_name(field).map(|n| self.text(n));
                found |= at("path") == Some(&table.ty) && at("name") == Some(&table.function);
            }
            Ok(found)
        })?;
        Ok(found)
    }

    /// The code that answers a table variant: the first arm outside the table function that
    /// names it, else the catch-all handler itself. `None` when that code serves a static
    /// asset: the arm's own expression, the function it delegates to, or the catch-all.
    fn dispatch_handler(
        &self,
        table: &Table,
        entry: &Entry,
        registered: &Declaration,
        arms: &[Arm<'a>],
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Handler>> {
        let variant = Some(entry.variant.as_str());
        let qualified = format!("{}::{}", table.ty, entry.variant);
        let own = format!("Self::{}", entry.variant);
        let mut answering = None;
        for arm in arms {
            let outside =
                arm.node.end_byte() <= table.span.start || table.span.end <= arm.node.start_byte();
            let names = arm.patterns.contains(&qualified)
                || (arm.self_type == Some(table.ty.as_str()) && arm.patterns.contains(&own));
            if outside && names && arm.answers && !self.excluded(arm.node) {
                answering = Some(arm.node);
                break;
            }
            check()?;
        }
        let Some(arm) = answering else {
            if let Some(body) = self.function_node(registered)
                && self.embeds(body, check)?
            {
                return Ok(None);
            }
            return Ok(Some(self.declaration_handler(registered, variant)));
        };
        let value = arm.child_by_field_name("value");
        if let Some(value) = value
            && self.embeds(value, check)?
        {
            return Ok(None);
        }
        let delegate = value
            .map(unwrap)
            .filter(|value| value.kind() == "call_expression")
            .and_then(|call| call.child_by_field_name("function"))
            .and_then(|function| self.function(function));
        if let Some(body) = delegate.and_then(|function| self.function_node(function))
            && self.embeds(body, check)?
        {
            return Ok(None);
        }
        Ok(Some(match delegate {
            Some(function) => self.declaration_handler(function, variant),
            None => self.span_handler(HandlerKind::DispatchArm, Self::range(arm), variant),
        }))
    }
}

/// `:name` is `{name}` and `*name` is `{*name}`.
fn normalize(literal: &str) -> String {
    literal
        .split('/')
        .map(
            |part| match (part.strip_prefix(':'), part.strip_prefix('*')) {
                (Some(name), _) => format!("{{{name}}}"),
                (_, Some(name)) => format!("{{*{name}}}"),
                _ => part.to_owned(),
            },
        )
        .collect::<Vec<_>>()
        .join("/")
}

/// Looks through a single-expression block, `.await` and `?`.
fn unwrap(mut node: Node<'_>) -> Node<'_> {
    loop {
        let inner = match node.kind() {
            "block" => {
                let mut cursor = node.walk();
                let mut children = node
                    .named_children(&mut cursor)
                    .filter(|child| !comment(*child));
                match (children.next(), children.next()) {
                    (Some(only), None) => Some(only),
                    _ => None,
                }
            }
            "await_expression" | "try_expression" => node.named_child(0),
            _ => None,
        };
        match inner {
            Some(inner) => node = inner,
            None => return node,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::{HandlerKind, Lines, Seg};
    use std::sync::{Arc, atomic::AtomicBool};

    fn snapshot(files: &[(&str, &str)]) -> Snapshot {
        let root = tempfile::tempdir().unwrap();
        for (path, text) in files {
            let path = root.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let identity =
            crate::store::topology::WorkspaceIdentity::discover(Some(root.path()), root.path())
                .unwrap();
        let (graph, native) = crate::indexer::index_workspace_with_native(
            &crate::indexer::IndexOptions::new(root.path().to_owned()),
            &identity.record_id,
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let documents = graph
            .files
            .into_iter()
            .map(|file| {
                let key = native
                    .revision
                    .documents
                    .iter()
                    .find(|d| d.key.path == file.path)
                    .unwrap()
                    .key
                    .clone();
                DocumentInput {
                    semantic: Default::default(),
                    declarations: native
                        .declarations
                        .iter()
                        .filter(|d| d.document == key)
                        .cloned()
                        .collect(),
                    calls: native
                        .calls
                        .iter()
                        .filter(|c| c.document == key)
                        .cloned()
                        .collect(),
                    key,
                    file,
                }
            })
            .collect();
        Snapshot {
            pin: crate::model::IndexPin {
                index_generation: uuid::Uuid::new_v4(),
                index_revision: 1,
            },
            documents,
            manifests: vec![],
            root_tsconfig: None,
            evidence_gaps: Default::default(),
        }
    }

    fn run(snapshot: &Snapshot) -> Vec<Route> {
        extract(snapshot, &mut Work::new(usize::MAX), &mut || Ok(())).unwrap()
    }

    fn routes(files: &[(&str, &str)]) -> Vec<Route> {
        run(&snapshot(files))
    }

    fn table(routes: &[Route]) -> Vec<String> {
        routes
            .iter()
            .map(|r| format!("{} {}", r.method, r.path_template))
            .collect()
    }

    fn find<'a>(routes: &'a [Route], method: &str, path: &str) -> &'a Route {
        routes
            .iter()
            .find(|r| r.method == method && r.path_template == path)
            .unwrap_or_else(|| panic!("no {method} {path} in {:?}", table(routes)))
    }

    fn declaration<'a>(snapshot: &'a Snapshot, name: &str) -> &'a Declaration {
        snapshot
            .documents
            .iter()
            .flat_map(|d| &d.declarations)
            .find(|d| d.name.as_deref() == Some(name))
            .unwrap()
    }

    fn slice<'a>(snapshot: &'a Snapshot, handler: &Handler) -> &'a str {
        let doc = snapshot
            .documents
            .iter()
            .find(|d| d.key == handler.document)
            .unwrap();
        &doc.file.text[handler.range.start..handler.range.end]
    }

    const LITERALS: &str = r#"
use axum::{routing::{get, post}, Router};
async fn health() {}
async fn start() {}
async fn job() {}
fn router() -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/api/index", post(start).put(start).get(health))
        .route("/api/jobs/:id/cancel", axum::routing::post(job))
        .route("/files/*rest", get(job).layer(x))
        .route("/inline", get(|| async { "x" }))
        .route("/", get(health))
}
"#;

    #[test]
    fn axum_literals_become_one_route_per_method_and_handler() {
        let found = routes(&[("src/http.rs", LITERALS)]);
        assert_eq!(
            table(&found),
            [
                "GET /healthz",
                "POST /api/index",
                "PUT /api/index",
                "GET /api/index",
                "POST /api/jobs/{id}/cancel",
                "GET /files/{*rest}",
                "GET /inline",
                "GET /",
            ]
        );
        assert!(found.iter().all(|r| !r.from_table));
        assert_eq!(
            find(&found, "POST", "/api/jobs/{id}/cancel").segments,
            [
                Seg::Literal("api".into()),
                Seg::Literal("jobs".into()),
                Seg::Param,
                Seg::Literal("cancel".into()),
            ]
        );
        assert_eq!(
            find(&found, "GET", "/files/{*rest}").segments,
            [Seg::Literal("files".into()), Seg::Rest]
        );
        assert!(find(&found, "GET", "/").segments.is_empty());
    }

    #[test]
    fn registrations_that_are_not_axum_literals_are_not_routes() {
        let source = r#"
fn router() {
    app.route("/one")
        .route("/a", get(h), extra)
        .route(path, get(h))
        .route("no-slash", get(h))
        .route("/esc\n", get(h))
        .route("/dyn", handler)
        .route("/ok", get(h));
}
"#;
        assert_eq!(table(&routes(&[("src/http.rs", source)])), ["GET /ok"]);
    }

    #[test]
    fn a_named_handler_is_a_declaration_and_anything_else_a_closure() {
        let shot = snapshot(&[("src/http.rs", LITERALS)]);
        let found = run(&shot);
        let health = find(&found, "GET", "/healthz").handler.clone();
        let function = declaration(&shot, "health");
        assert_eq!(health.kind, HandlerKind::Declaration);
        assert_eq!(
            health.syntax_id.as_deref(),
            Some(function.syntax_id.as_str())
        );
        assert_eq!(
            health.stable_lookup_key.as_deref(),
            Some("src/http.rs::health")
        );
        assert_eq!(health.range, function.range);
        assert_eq!(health.lines, Lines { start: 3, end: 3 });
        assert_eq!(health.variant, None);

        let closure = &find(&found, "GET", "/inline").handler;
        let router = declaration(&shot, "router");
        assert_eq!(closure.kind, HandlerKind::Closure);
        assert_eq!(
            closure.syntax_id.as_deref(),
            Some(router.syntax_id.as_str())
        );
        assert_eq!(
            closure.stable_lookup_key.as_deref(),
            Some("src/http.rs::router")
        );
        assert_eq!(slice(&shot, closure), "|| async { \"x\" }");
        assert_eq!(closure.lines, Lines { start: 12, end: 12 });
    }

    const TABLE: &str = r#"
use axum::http::Method;
enum Core { Git, Metrics, Status, Tree, Files, Job }
async fn core(m: Method, s: String) {
    let route = Core::parse(&m, &s);
    match route { _ => {} }
}
async fn metrics_answer() {}
async fn status_answer() {}
async fn files_answer() {}
async fn job_answer() {}
async fn answer(route: Core) {
    match route {
        Core::Git => {
            unreachable!("git bypasses")
        }
        Core::Metrics => { metrics_answer() }
        Core::Status => status_answer().await?,
        Core::Tree => {
            let q = query();
            tree_answer(q)
        }
        Core::Files => files_answer().await,
        Core::Job => job_answer(),
    }
}
impl Core {
    fn parse(method: &Method, suffix: &str) -> Option<Self> {
        match (method, suffix) {
            (&Method::GET, "git") => Some(Self::Git),
            (&Method::GET, "metrics") => Some(Self::Metrics),
            (&Method::GET, "status") => Some(Self::Status),
            (&Method::POST, "tree") => Some(Self::Tree),
            (&axum::http::Method::GET, "files") => Some(Core::Files),
            _ => {
                let (family, tail) = suffix.split_once('/')?;
                match (method, family) {
                    (&Method::GET, "jobs") if tail.len() > 1 => Some(Self::Job),
                    (&Method::POST, "jobs") => {
                        let id = tail.strip_suffix("/cancel")?;
                        Some(Self::Job)
                    }
                    _ => None,
                }
            }
        }
    }
}
fn router() -> Router {
    Router::new().route("/api/checkouts/:root_key/*suffix", get(core).post(core))
}
"#;

    #[test]
    fn a_catch_all_whose_handler_calls_a_table_expands_to_the_table() {
        let found = routes(&[("src/http.rs", TABLE)]);
        assert_eq!(
            table(&found),
            [
                "GET /api/checkouts/{root_key}/git",
                "GET /api/checkouts/{root_key}/metrics",
                "GET /api/checkouts/{root_key}/status",
                "GET /api/checkouts/{root_key}/files",
                "GET /api/checkouts/{root_key}/jobs/{*}",
                "POST /api/checkouts/{root_key}/tree",
                "POST /api/checkouts/{root_key}/jobs/{*}",
            ]
        );
        assert!(found.iter().all(|r| r.from_table));
        assert_eq!(
            find(&found, "POST", "/api/checkouts/{root_key}/jobs/{*}").segments,
            [
                Seg::Literal("api".into()),
                Seg::Literal("checkouts".into()),
                Seg::Param,
                Seg::Literal("jobs".into()),
                Seg::Rest,
            ]
        );
    }

    #[test]
    fn a_table_method_the_registration_lacks_is_dropped() {
        let source = TABLE.replace("get(core).post(core)", "get(core)");
        let found = routes(&[("src/http.rs", &source)]);
        assert!(found.iter().all(|r| r.method == "GET"));
        assert_eq!(found.len(), 5);
    }

    #[test]
    fn a_catch_all_that_calls_no_table_is_an_ordinary_route() {
        let source = TABLE.replace("Core::parse(&m, &s)", "other(&m, &s)");
        let found = routes(&[("src/http.rs", &source)]);
        let route = find(&found, "GET", "/api/checkouts/{root_key}/{*suffix}");
        assert!(!route.from_table);
        assert_eq!(route.handler.kind, HandlerKind::Declaration);
        assert_eq!(found.len(), 2);
    }

    #[test]
    fn an_answering_arm_delegating_to_one_function_is_a_declaration() {
        let shot = snapshot(&[("src/http.rs", TABLE)]);
        let found = run(&shot);
        for (path, name, variant) in [
            (
                "GET /api/checkouts/{root_key}/metrics",
                "metrics_answer",
                "Metrics",
            ),
            (
                "GET /api/checkouts/{root_key}/status",
                "status_answer",
                "Status",
            ),
            (
                "GET /api/checkouts/{root_key}/files",
                "files_answer",
                "Files",
            ),
            (
                "GET /api/checkouts/{root_key}/jobs/{*}",
                "job_answer",
                "Job",
            ),
        ] {
            let (method, path) = path.split_once(' ').unwrap();
            let handler = &find(&found, method, path).handler;
            let function = declaration(&shot, name);
            assert_eq!(handler.kind, HandlerKind::Declaration, "{path}");
            assert_eq!(
                handler.syntax_id.as_deref(),
                Some(function.syntax_id.as_str())
            );
            assert_eq!(handler.range, function.range);
            assert_eq!(handler.variant.as_deref(), Some(variant));
        }
    }

    #[test]
    fn an_inline_arm_is_a_dispatch_arm_of_the_enclosing_callable() {
        let shot = snapshot(&[("src/http.rs", TABLE)]);
        let found = run(&shot);
        let handler = &find(&found, "POST", "/api/checkouts/{root_key}/tree").handler;
        let answer = declaration(&shot, "answer");
        assert_eq!(handler.kind, HandlerKind::DispatchArm);
        // The published spelling: no route of Trellis's own router is an inline arm any
        // more (#1046), so this fixture is what pins it.
        assert_eq!(
            serde_json::to_value(handler.kind).unwrap(),
            serde_json::json!("dispatchArm")
        );
        assert_eq!(
            handler.syntax_id.as_deref(),
            Some(answer.syntax_id.as_str())
        );
        assert_eq!(
            handler.stable_lookup_key.as_deref(),
            Some("src/http.rs::answer")
        );
        assert_eq!(handler.variant.as_deref(), Some("Tree"));
        assert!(slice(&shot, handler).starts_with("Core::Tree => {"));
        assert!(slice(&shot, handler).ends_with("tree_answer(q)\n        }"));
    }

    #[test]
    fn a_variant_no_arm_answers_falls_back_to_the_catch_all_handler() {
        let shot = snapshot(&[("src/http.rs", TABLE)]);
        let found = run(&shot);
        let handler = &find(&found, "GET", "/api/checkouts/{root_key}/git").handler;
        let core = declaration(&shot, "core");
        assert_eq!(handler.kind, HandlerKind::Declaration);
        assert_eq!(handler.syntax_id.as_deref(), Some(core.syntax_id.as_str()));
        assert_eq!(handler.variant.as_deref(), Some("Git"));
    }

    #[test]
    fn an_arm_that_is_a_statement_or_a_wrapped_value_is_not_unwrapped() {
        let source = TABLE.replace(
            "Core::Job => job_answer(),",
            "Core::Job => { job_answer(); }",
        );
        let shot = snapshot(&[("src/http.rs", &source)]);
        let found = run(&shot);
        let handler = &find(&found, "GET", "/api/checkouts/{root_key}/jobs/{*}").handler;
        assert_eq!(handler.kind, HandlerKind::DispatchArm);
        let source = TABLE.replace(
            "Core::Job => job_answer(),",
            "Core::Job => Ok(job_answer()),",
        );
        let shot = snapshot(&[("src/http.rs", &source)]);
        let found = run(&shot);
        let handler = &find(&found, "GET", "/api/checkouts/{root_key}/jobs/{*}").handler;
        assert_eq!(handler.kind, HandlerKind::DispatchArm);
    }

    #[test]
    fn test_role_code_is_not_extracted() {
        let source = r#"
async fn h() {}
fn router() -> Router { Router::new().route("/prod", get(h)) }
#[cfg(test)]
mod tests {
    fn fixture() -> Router { Router::new().route("/in-test-module", get(h)) }
}
"#;
        assert_eq!(
            table(&routes(&[
                ("src/http.rs", source),
                ("tests/http.rs", source),
                ("src/http_tests.rs", source),
            ])),
            ["GET /prod"]
        );
    }

    #[test]
    fn the_test_support_attribute_excludes_items_statements_and_arms_but_only_exactly() {
        let source = r#"
async fn h() {}
#[cfg(feature = "test-support")]
fn a() -> Router { Router::new().route("/item", get(h)) }
#[cfg(
    feature   =
        "test-support"
)]
pub fn b() -> Router { Router::new().route("/irregular", get(h)) }
fn c() -> Router {
    #[cfg( feature="test-support" )]
    let r = Router::new().route("/statement", get(h));
    Router::new().route("/kept", get(h))
}
#[cfg(feature = "other")]
fn d() -> Router { Router::new().route("/other-feature", get(h)) }
fn f() -> Router {
    #[cfg(feature = "test-support")]
    {
        Router::new().route("/block", get(h))
    }
}
"#;
        let found = table(&routes(&[("src/http.rs", source)]));
        assert_eq!(found, ["GET /kept", "GET /other-feature"]);
    }

    #[test]
    fn a_path_qualified_panic_macro_arm_does_not_answer() {
        for macro_call in [
            "std::panic!(\"x\")",
            "core::unreachable!(\"x\")",
            "std::todo!()",
            "core::unimplemented!()",
        ] {
            let source = TABLE.replace("unreachable!(\"git bypasses\")", macro_call);
            let shot = snapshot(&[("src/http.rs", &source)]);
            let found = run(&shot);
            let handler = &find(&found, "GET", "/api/checkouts/{root_key}/git").handler;
            assert_eq!(handler.kind, HandlerKind::Declaration, "{macro_call}");
            assert_eq!(handler.variant.as_deref(), Some("Git"));
        }
    }

    #[test]
    fn a_dispatch_arm_under_the_test_support_attribute_does_not_answer() {
        let source = TABLE.replace(
            "        Core::Files => files_answer().await,",
            "        #[cfg(feature = \"test-support\")]\n        Core::Files => other_answer(),\n        Core::Files => files_answer().await,",
        );
        let shot = snapshot(&[("src/http.rs", &source)]);
        let found = run(&shot);
        let handler = &find(&found, "GET", "/api/checkouts/{root_key}/files").handler;
        assert_eq!(handler.kind, HandlerKind::Declaration);
        assert_eq!(
            handler.syntax_id.as_deref(),
            Some(declaration(&shot, "files_answer").syntax_id.as_str())
        );
    }

    #[test]
    fn static_assets_are_not_routes() {
        let source = r#"
async fn page() -> &'static str { include_str!("../web/index.html") }
async fn logo() -> &'static [u8] { std::include_bytes!("../web/logo.png") }
async fn api() {}
fn router() -> Router {
    Router::new()
        .route("/", get(|| async { ([(h, "x")], include_str!("../web/index.html")) }))
        .route("/page", get(page))
        .route("/logo", get(logo))
        .route("/api", get(api))
}
"#;
        assert_eq!(table(&routes(&[("src/http.rs", source)])), ["GET /api"]);
    }

    #[test]
    fn one_route_per_method_and_template_first_location_wins() {
        let a = "async fn h() {}\nfn r() { x.route(\"/same\", get(h)).route(\"/same\", post(h)).route(\"/same\", get(h)); }";
        let b = "async fn g() {}\nfn r() { x.route(\"/same\", get(g)); }";
        let found = routes(&[("src/z.rs", b), ("src/a.rs", a)]);
        assert_eq!(table(&found), ["GET /same", "POST /same"]);
        assert_eq!(
            find(&found, "GET", "/same").handler.document.path,
            "src/a.rs"
        );
    }

    #[test]
    fn evidence_gaps_and_other_languages_are_skipped() {
        let mut shot = snapshot(&[
            (
                "src/gap.rs",
                "async fn h() {}\nfn r() { x.route(\"/gap\", get(h)); }",
            ),
            (
                "src/ok.rs",
                "async fn h() {}\nfn r() { x.route(\"/ok\", get(h)); }",
            ),
            ("web/app.js", "x.route('/js', get(h));"),
        ]);
        let gap = shot
            .documents
            .iter()
            .find(|d| d.key.path == "src/gap.rs")
            .unwrap()
            .key
            .clone();
        shot.evidence_gaps.insert(gap, "partial");
        assert_eq!(table(&run(&shot)), ["GET /ok"]);
    }

    #[test]
    fn only_documents_that_can_register_routes_are_parsed_and_charged() {
        let shot = snapshot(&[
            ("src/plain.rs", "fn f() {}"),
            (
                "src/one.rs",
                "async fn h() {}\nfn r() { x.route(\"/a\", get(h)); }",
            ),
        ]);
        let mut work = Work::new(usize::MAX);
        extract(&shot, &mut work, &mut || Ok(())).unwrap();
        let one = shot
            .documents
            .iter()
            .find(|d| d.key.path == "src/one.rs")
            .unwrap();
        assert_eq!(work.used, one.file.text.len());
    }

    #[test]
    fn an_exhausted_budget_is_typed() {
        let shot = snapshot(&[(
            "src/one.rs",
            "async fn h() {}\nfn r() { x.route(\"/a\", get(h)); }",
        )]);
        let error = extract(&shot, &mut Work::new(4), &mut || Ok(())).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("bounded_work: interface extraction budget")
        );
    }

    #[test]
    fn a_stopped_check_ends_extraction() {
        let shot = snapshot(&[(
            "src/one.rs",
            "async fn h() {}\nfn r() { x.route(\"/a\", get(h)); }",
        )]);
        let error = extract(&shot, &mut Work::new(usize::MAX), &mut || {
            anyhow::bail!("cancelled")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "cancelled");
    }

    #[test]
    fn malformed_and_deeply_nested_source_does_not_panic() {
        let deep = format!(
            "fn r() {{ x.route(\"/a\", {}h{}) ; }}",
            "get(".repeat(3000),
            ")".repeat(3000)
        );
        let nested = format!("fn r() {{ {} x.route(\"/b\", get(h)) }}", "{".repeat(5000));
        let chain = format!("fn r() {{ x{}; }}", ".route(\"/c\", get(h))".repeat(2000));
        let broken =
            "impl T { fn f( { match (&Method::GET, \"x\") { (&Method::GET, \"x\") => Self::";
        for source in [deep, nested, chain, broken.to_owned()] {
            let _ = routes(&[("src/http.rs", &source)]);
        }
    }

    const ACTIX_ATTRIBUTES: &str = r#"
use actix_web::{get, post, route, web, App, HttpResponse};
#[get("/health")]
async fn health() -> HttpResponse { HttpResponse::Ok().finish() }
#[actix_web::post(r"/users/{id:\d+}")]
async fn update(path: web::Path<u32>) -> HttpResponse { todo() }
/// Documented.
#[route("/both", method = "GET", method = "PUT")]
async fn both() -> HttpResponse { todo() }
#[get("/files/{tail:.*}")]
#[delete("/files/{tail:.*}")]
async fn files() -> HttpResponse { todo() }
#[other("/not-a-route")]
async fn other() {}
#[get("/page")]
async fn page() -> &'static str { include_str!("index.html") }
#[cfg(test)]
mod tests {
    #[get("/in-test")]
    async fn fixture() {}
}
"#;

    #[test]
    fn actix_route_attributes_are_routes_of_their_function() {
        let shot = snapshot(&[("src/main.rs", ACTIX_ATTRIBUTES)]);
        let found = run(&shot);
        assert_eq!(
            table(&found),
            [
                "GET /health",
                "POST /users/{id}",
                "GET /both",
                "PUT /both",
                "GET /files/{*tail}",
                "DELETE /files/{*tail}",
            ]
        );
        let health = &find(&found, "GET", "/health").handler;
        assert_eq!(health.kind, HandlerKind::Declaration);
        assert_eq!(
            health.syntax_id.as_deref(),
            Some(declaration(&shot, "health").syntax_id.as_str())
        );
        assert_eq!(
            health.stable_lookup_key.as_deref(),
            Some("src/main.rs::health")
        );
        assert_eq!(
            find(&found, "GET", "/files/{*tail}").segments,
            [Seg::Literal("files".into()), Seg::Rest]
        );
    }

    const ACTIX_BUILDERS: &str = r#"
use actix_web::{web, App, HttpServer, http::Method};
async fn index() {}
async fn create() {}
async fn show() {}
async fn remove() {}
async fn version() {}
async fn status() {}
async fn guarded() {}
#[actix_web::get("/me")]
async fn me() {}
#[actix_web::get("/{id}")]
async fn item() {}
fn app() -> App {
    App::new()
        .route("/", web::get().to(index))
        .route("/items", web::post().to(create))
        .route("/inline", web::get().to(|| async { "x" }))
        .route("/method", web::method(Method::PATCH).to(index))
        .route("/any", web::route().method(Method::DELETE).guard(g).to(guarded))
        .route("/all-methods", web::route().to(index))
        .service(web::resource("/items/{id}").route(web::get().to(show)).route(web::delete().to(remove)))
        .service(
            web::scope("/api")
                .route("/version", web::get().to(version))
                .service(me)
                .service(
                    web::scope("/v1/")
                        .service(web::resource("status").route(web::get().to(status)))
                        .service(handlers::item),
                ),
        )
        .service(web::scope(PREFIX).route("/lost", web::get().to(index)))
}
"#;

    #[test]
    fn actix_builders_and_nested_scopes_are_routes() {
        let shot = snapshot(&[("src/main.rs", ACTIX_BUILDERS)]);
        let found = run(&shot);
        assert_eq!(
            table(&found),
            [
                "GET /api/me",
                "GET /api/v1/{id}",
                "GET /",
                "POST /items",
                "GET /inline",
                "PATCH /method",
                "DELETE /any",
                "GET /items/{id}",
                "DELETE /items/{id}",
                "GET /api/version",
                "GET /api/v1/status",
            ]
        );
        let show = &find(&found, "GET", "/items/{id}").handler;
        assert_eq!(show.kind, HandlerKind::Declaration);
        assert_eq!(
            show.syntax_id.as_deref(),
            Some(declaration(&shot, "show").syntax_id.as_str())
        );
        let inline = &find(&found, "GET", "/inline").handler;
        assert_eq!(inline.kind, HandlerKind::Closure);
        assert_eq!(slice(&shot, inline), "|| async { \"x\" }");
        assert_eq!(
            find(&found, "GET", "/api/me").handler.syntax_id.as_deref(),
            Some(declaration(&shot, "me").syntax_id.as_str())
        );
    }

    #[test]
    fn an_actix_service_in_another_document_scopes_the_attribute_route() {
        let handlers = "use actix_web::get;\n#[get(\"/{id}\")]\npub async fn item() {}\n#[get(\"/free\")]\npub async fn free() {}\n";
        let main = "use actix_web::{web, App};\nfn app() -> App {\n    App::new().service(web::scope(\"/a\").service(handlers::item)).service(web::scope(\"/b\").service(item))\n}\n";
        let found = routes(&[("src/handlers.rs", handlers), ("src/main.rs", main)]);
        assert_eq!(table(&found), ["GET /a/{id}", "GET /b/{id}", "GET /free"]);
        assert!(
            found
                .iter()
                .all(|route| route.handler.document.path == "src/handlers.rs")
        );
    }

    #[test]
    fn actix_test_code_and_documents_without_actix_are_not_routes() {
        let source = "use actix_web::{web, App};\nasync fn h() {}\nfn app() -> App { App::new().route(\"/prod\", web::get().to(h)) }\n#[cfg(feature = \"test-support\")]\nfn fixture() -> App { App::new().route(\"/support\", web::get().to(h)) }\n";
        let unrelated = "#[get(\"/rocket\")]\nfn rocket() {}\nfn f(map: Map) { map.route(\"/x\", other().to(h)); }\n";
        assert_eq!(
            table(&routes(&[
                ("src/main.rs", source),
                ("tests/api.rs", source),
                ("src/rocket.rs", unrelated),
            ])),
            ["GET /prod"]
        );
    }

    #[test]
    fn an_actix_document_is_charged_and_an_exhausted_budget_is_typed() {
        let shot = snapshot(&[("src/main.rs", ACTIX_ATTRIBUTES)]);
        let error = extract(
            &shot,
            &mut Work::new(ACTIX_ATTRIBUTES.len() - 1),
            &mut || Ok(()),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), crate::interfaces::BUDGET);
        // After the parse: six attributed routes, each expanded once at its own path, charged
        // as one unit plus its path bytes and two (#867 review).
        let paths = [
            "/health",
            r"/users/{id:\d+}",
            "/both",
            "/both",
            "/files/{tail:.*}",
            "/files/{tail:.*}",
        ];
        let expansion: usize = paths.iter().map(|path| path.len() + 2).sum();
        let exact = ACTIX_ATTRIBUTES.len() + 6 + expansion;
        assert_eq!(run(&shot).len(), 6);
        extract(&shot, &mut Work::new(exact), &mut || Ok(())).unwrap();
        let error = extract(&shot, &mut Work::new(exact - 1), &mut || Ok(())).unwrap_err();
        assert_eq!(error.to_string(), crate::interfaces::BUDGET);
    }

    /// #867 review P2: a registration whose receiver is not a proven `App` or an inline scope
    /// or resource registers nothing, and `.configure(..)` is not followed.
    #[test]
    fn an_actix_registration_on_an_unknown_receiver_registers_nothing() {
        let source = r#"
use actix_web::{get, web, App};
async fn h() {}
#[get("/attributed")]
async fn attributed() {}
fn app(prefix: &str) {
    let scoped = web::scope(prefix);
    let scoped = scoped.route("/lost", web::get().to(h)).service(attributed);
    let _app = App::new().service(scoped);
    let _configured = App::new().configure(|cfg| {
        cfg.route("/configured", web::get().to(h));
    });
    let _kept = actix_web::App::new().route("/kept", web::get().to(h));
}
"#;
        assert_eq!(table(&routes(&[("src/main.rs", source)])), ["GET /kept"]);
    }

    /// #867 review P2: attributed routes times the scopes that register them is charged
    /// before it is built.
    #[test]
    fn the_attributed_route_and_service_fan_out_is_charged() {
        let mut files: Vec<(String, String)> = (0..20)
            .map(|n| {
                (
                    format!("src/m{n}.rs"),
                    format!("use actix_web::get;\n#[get(\"/p{n}\")]\npub async fn item() {{}}\n"),
                )
            })
            .collect();
        let services: String = (0..20)
            .map(|n| format!(".service(web::scope(\"/s{n}\").service(m{n}::item))"))
            .collect();
        files.push((
            "src/main.rs".into(),
            format!("use actix_web::{{web, App}};\nfn app() {{ App::new(){services}; }}\n"),
        ));
        let files: Vec<(&str, &str)> = files
            .iter()
            .map(|(path, text)| (path.as_str(), text.as_str()))
            .collect();
        let shot = snapshot(&files);
        assert_eq!(run(&shot).len(), 400);
        let bytes: usize = shot.documents.iter().map(|d| d.file.text.len()).sum();
        let error = extract(&shot, &mut Work::new(bytes + 40), &mut || Ok(())).unwrap_err();
        assert_eq!(error.to_string(), crate::interfaces::BUDGET);
    }

    #[test]
    fn segments_split_and_drop_a_trailing_empty_segment() {
        assert_eq!(
            segments("/a/{b}/{*c}/d{e}/"),
            [Seg::Literal("a".into()), Seg::Param, Seg::Rest, Seg::Param,]
        );
        assert!(segments("/").is_empty());
    }

    #[test]
    fn lines_are_one_based_and_inclusive() {
        let text = "ab\ncd\n\nef";
        let index = LineIndex::new(text);
        let lines = |start, end| index.lines(&Range { start, end });
        assert_eq!(lines(0, 2), Lines { start: 1, end: 1 });
        assert_eq!(lines(0, 3), Lines { start: 1, end: 1 });
        assert_eq!(lines(3, 9), Lines { start: 2, end: 4 });
        assert_eq!(lines(6, 6), Lines { start: 3, end: 3 });
    }
}
