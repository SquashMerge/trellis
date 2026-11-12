//! Rust client calls (#858): `reqwest` requests whose URL is a literal, a `format!` with a
//! readable path, or a same-document string constant. Anything else is an unreadable path.
use super::calls::client_segments;
use super::js::verb;
use super::{
    ClientCall, LineIndex, charge, comment, routes::path_verb, test_span, under_test_support,
};
use crate::native_evidence::Range;
use crate::references::{DocumentInput, callers::Callers, walk, work::Work};
use anyhow::Result;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

/// The request builders of a `reqwest` client that take only the URL.
const BUILDERS: [&str; 6] = ["get", "post", "put", "delete", "patch", "head"];
/// The free functions that send a `GET`.
const FREE: [&str; 2] = ["reqwest::get", "reqwest::blocking::get"];
/// Stands for a substitution while a URL is assembled.
pub(super) const UNKNOWN: char = '\0';

/// The `reqwest` calls of every production Rust document.
pub(super) fn extract(
    snapshot: &crate::references::Snapshot,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<ClientCall>> {
    let mut found = Vec::new();
    for doc in &snapshot.documents {
        check()?;
        if !candidate(doc) || snapshot.evidence_gaps.contains_key(&doc.key) || super::test_file(doc)
        {
            continue;
        }
        charge(work, doc.file.text.len())?;
        let tree = super::parse(doc, &tree_sitter_rust::LANGUAGE.into(), check)?;
        collect(doc, tree.root_node(), &mut found, work, check)?;
    }
    Ok(found)
}

/// Whether a Rust document can make `reqwest` calls.
fn candidate(doc: &DocumentInput) -> bool {
    doc.key.language == "rust" && doc.file.text.contains("reqwest")
}

/// The `reqwest` calls of one document, in source order.
fn collect(
    doc: &DocumentInput,
    root: Node<'_>,
    found: &mut Vec<ClientCall>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let document = Document {
        source: &doc.file.text,
        constants: BTreeMap::new(),
        clients: BTreeSet::new(),
        fields: BTreeMap::new(),
        bindings: BTreeMap::new(),
        globs: Vec::new(),
    };
    // A name declared more than once anywhere may be shadowed, so it is never read. Only a
    // top-level declaration is read, and only from outside every inline module. Every
    // identifier under an import or a binding pattern counts as another declaration
    // (#867 review): a local `use .. as NAME` hides the outer item.
    let mut constants: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut bound: BTreeSet<&str> = BTreeSet::new();
    let mut local_glob = false;
    let mut modules = Vec::new();
    let mut sites = Vec::new();
    // How often each type-namespace name is declared anywhere in the document, and the
    // names a top-level `use reqwest::..` gives the client (#910, precision over recall).
    let mut declared: BTreeMap<&str, usize> = BTreeMap::new();
    let mut imported = BTreeSet::new();
    let mut fields: BTreeMap<&str, (usize, bool, Option<&str>)> = BTreeMap::new();
    let mut globs = Vec::new();
    let mut top_glob = false;
    let mut field_nodes = Vec::new();
    let mut bindings: BTreeMap<&str, Vec<Node<'_>>> = BTreeMap::new();
    walk(root, check, &mut |node, check| {
        let binding = match node.kind() {
            "use_declaration" | "closure_parameters" => Some(node),
            "parameter" | "let_declaration" | "match_arm" | "for_expression" | "let_condition" => {
                node.child_by_field_name("pattern")
            }
            _ => None,
        };
        if let Some(binding) = binding {
            // A glob below the top level may bind any name. A top-level glob is shadowed by
            // the top-level constant itself, as an item shadows a glob import.
            let local = node.kind() == "use_declaration"
                && node.parent().is_some_and(|parent| parent.id() != root.id());
            walk(binding, check, &mut |inner, _| {
                match inner.kind() {
                    // Every name the pattern binds, a shorthand field (`Slot { c }`) too.
                    "identifier" | "shorthand_field_identifier" => {
                        bound.insert(document.text(inner));
                        bindings
                            .entry(document.text(inner))
                            .or_default()
                            .push(binding);
                    }
                    "use_wildcard" if local => {
                        local_glob = true;
                        globs.push(binding.byte_range());
                    }
                    "use_wildcard" => top_glob = true,
                    _ => {}
                }
                Ok(false)
            })?;
        }
        let kind = node.kind();
        // An item also binds its name for the locals of a function that holds it: a block's
        // `const c` hides a parameter `c`.
        if kind.ends_with("_item")
            && let Some(name) = node.child_by_field_name("name")
        {
            *declared.entry(document.text(name)).or_default() += 1;
            bindings.entry(document.text(name)).or_default().push(node);
        }
        match kind {
            "mod_item" => modules.push(node.byte_range()),
            "use_declaration" => {
                let top = node.parent().is_some_and(|parent| parent.id() == root.id());
                for (name, client) in document.imports(node) {
                    *declared.entry(name).or_default() += 1;
                    if client && top {
                        imported.insert(name);
                    }
                }
            }
            // Every name anywhere in a generic parameter list, bounds included.
            "type_parameters" => {
                walk(node, check, &mut |inner, _| {
                    if matches!(inner.kind(), "identifier" | "type_identifier") {
                        *declared.entry(document.text(inner)).or_default() += 1;
                    }
                    Ok(false)
                })?;
            }
            "field_declaration" => field_nodes.push(node),
            "const_item" | "static_item" => {
                let name = node.child_by_field_name("name").map(|n| document.text(n));
                let value = node
                    .child_by_field_name("value")
                    .filter(|_| node.parent().is_some_and(|parent| parent.id() == root.id()))
                    .and_then(|value| document.literal(value));
                if let Some(name) = name {
                    constants
                        .entry(name.to_owned())
                        .and_modify(|seen| *seen = None)
                        .or_insert(value);
                }
            }
            "call_expression" => sites.push(node),
            _ => {}
        }
        Ok(false)
    })?;
    // An imported client name counts only when nothing else in the document declares it,
    // and no glob below the top level may bring in another type of that name.
    let document = Document {
        clients: imported
            .into_iter()
            .filter(|name| !local_glob && declared.get(name) == Some(&1))
            .collect(),
        ..document
    };
    // A field counts only when it is the one field of that name in the document, declared
    // with the client type by a top-level struct whose name the document declares once and
    // never imports, in a document with no top-level glob import (#921 review): a top-level
    // glob or a module could make the impl's name another type. A glob inside a nested
    // module cannot, since names never resolve outward from a child module.
    for field in field_nodes {
        if let Some(name) = field.child_by_field_name("name") {
            let client = field
                .child_by_field_name("type")
                .is_some_and(|ty| document.client_type(ty));
            let owner = field
                .parent()
                .and_then(|list| list.parent())
                .filter(|item| item.kind() == "struct_item" && top_level(*item))
                .and_then(|item| item.child_by_field_name("name"))
                .map(|owner| document.text(owner));
            let entry = fields
                .entry(document.text(name))
                .or_insert((0, true, owner));
            entry.0 += 1;
            entry.1 &= client;
        }
    }
    let document = Document {
        fields: fields
            .into_iter()
            .filter_map(|(name, (count, client, owner))| {
                let owner = owner.filter(|owner| !top_glob && declared.get(owner) == Some(&1))?;
                (count == 1 && client).then_some((name, owner))
            })
            .collect(),
        bindings,
        globs,
        constants: constants
            .into_iter()
            .filter(|(name, _)| !local_glob && !bound.contains(name.as_str()))
            .filter_map(|(name, value)| Some((name, value?)))
            .collect(),
        ..document
    };
    let lines = LineIndex::new(document.source);
    let callers = Callers::new(doc);
    let mut calls = Vec::new();
    for node in sites {
        check()?;
        let range = Range {
            start: node.start_byte(),
            end: node.end_byte(),
        };
        if node.has_error() || test_span(doc, &range) || under_test_support(node, document.source) {
            continue;
        }
        let Some((method, url)) = document.request(node) else {
            continue;
        };
        let visible = !modules
            .iter()
            .any(|module| module.start <= range.start && range.end <= module.end);
        let template = document.url(url, visible, work, check)?;
        let call_id = doc
            .calls
            .iter()
            .find(|call| call.range == range)
            .map(|call| call.id.clone());
        calls.push(ClientCall {
            prefix: super::ClientPrefix::Written,
            method: method.map(str::to_owned),
            segments: template.as_deref().map(client_segments),
            path_template: template,
            document: doc.key.clone(),
            lines: lines.lines(&range),
            caller: callers.get(call_id.as_deref(), &range).0,
            call_id,
            range,
        });
    }
    calls.sort_by_key(|call| (call.range.start, call.range.end));
    found.extend(calls);
    Ok(())
}

struct Document<'a> {
    source: &'a str,
    /// Same-document `const` and `static` strings, each name declared once.
    constants: BTreeMap<String, String>,
    /// The names a top-level `use reqwest::..` gives the client that nothing else in the
    /// document declares.
    clients: BTreeSet<&'a str>,
    /// The field names declared once in the document, with the client type, each with the
    /// struct that declares it.
    fields: BTreeMap<&'a str, &'a str>,
    /// Every pattern that binds each name: a parameter, `let`, closure parameter, `match`,
    /// `if let`, `while let` or `for` pattern, an import, or a named item.
    bindings: BTreeMap<&'a str, Vec<Node<'a>>>,
    /// The byte ranges of the glob imports below the top level.
    globs: Vec<std::ops::Range<usize>>,
}

impl<'a> Document<'a> {
    fn text(&self, node: Node<'_>) -> &'a str {
        self.source.get(node.byte_range()).unwrap_or("")
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
        (!text.contains(UNKNOWN)).then_some(text)
    }

    /// The method and URL argument of a `reqwest` request. The method is `None` when it
    /// cannot be read.
    fn request(&self, call: Node<'a>) -> Option<(Option<&'static str>, Node<'a>)> {
        let function = call.child_by_field_name("function")?;
        let arguments = Self::arguments(call);
        match function.kind() {
            "scoped_identifier" => {
                let path: String = self.text(function).split_whitespace().collect();
                match &arguments[..] {
                    [url] if FREE.contains(&path.as_str()) => Some((Some("GET"), *url)),
                    _ => None,
                }
            }
            "field_expression" => {
                let field = self.text(function.child_by_field_name("field")?);
                if !self.client(function.child_by_field_name("value")?) {
                    return None;
                }
                match (field, &arguments[..]) {
                    (builder, [url]) if BUILDERS.contains(&builder) => Some((verb(builder), *url)),
                    ("request", [method, url]) => {
                        let method: String = self.text(*method).split_whitespace().collect();
                        Some((path_verb(method.trim_start_matches('&')), *url))
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Every name a `use` declaration binds, and whether it is the client: rooted at
    /// `reqwest` and naming `Client`, aliased or not (`Client`, `Client as Http`). A glob
    /// binds no name it can be known by.
    fn imports(&self, declaration: Node<'a>) -> Vec<(&'a str, bool)> {
        let Some(argument) = declaration.child_by_field_name("argument") else {
            return Vec::new();
        };
        let path: String = self.text(argument).split_whitespace().collect();
        let rooted = path.trim_start_matches("::").starts_with("reqwest::");
        let client =
            |original: &str| rooted && (original == "Client" || original.ends_with("::Client"));
        let mut names = Vec::new();
        let mut stack = vec![argument];
        while let Some(node) = stack.pop() {
            match node.kind() {
                "use_as_clause" => {
                    let original = node.child_by_field_name("path").map(|p| self.text(p));
                    if let Some(alias) = node.child_by_field_name("alias") {
                        names.push((self.text(alias), original.is_some_and(client)));
                    }
                }
                "scoped_identifier" => {
                    if let Some(name) = node.child_by_field_name("name") {
                        let name = self.text(name);
                        names.push((name, client(name)));
                    }
                }
                "identifier" => {
                    let name = self.text(node);
                    names.push((name, client(name)));
                }
                // The path of `a::{..}` is a prefix; only its list binds.
                "scoped_use_list" => stack.extend(node.child_by_field_name("list")),
                _ => stack.extend(Self::named_children(node)),
            }
        }
        names
    }

    /// Whether a written type is the `reqwest` client, seen through references:
    /// `reqwest::Client`, `reqwest::blocking::Client`, or a name a top-level reqwest import
    /// gives the client and nothing else in the document declares.
    fn client_type(&self, ty: Node<'_>) -> bool {
        let mut ty = ty;
        while ty.kind() == "reference_type" {
            let Some(inner) = ty.child_by_field_name("type") else {
                return false;
            };
            ty = inner;
        }
        let text: String = self.text(ty).split_whitespace().collect();
        let text = text.trim_start_matches("::");
        matches!(text, "reqwest::Client" | "reqwest::blocking::Client")
            || self.clients.contains(text)
    }

    /// Whether a receiver is unambiguously a `reqwest` client (#910). Precision wins over
    /// recall: a missed call is acceptable, a false one is not. The receiver is a
    /// constructor chain ([`Self::constructed`]); `self.<field>` inside a top-level `impl`
    /// of the top-level struct that declares the field, when that is the one field of its
    /// name in the document, has the client type, the struct's name is declared once and
    /// imported nowhere, and the document has no top-level glob import; or a name
    /// bound once in its function, by a parameter or `let` written with the client type,
    /// before the call. Its name says nothing.
    fn client(&self, receiver: Node<'a>) -> bool {
        match receiver.kind() {
            "identifier" => self.bound_client(receiver),
            "field_expression" => {
                let owner = receiver
                    .child_by_field_name("field")
                    .and_then(|field| self.fields.get(self.text(field)));
                let (Some(owner), Some(value)) = (owner, receiver.child_by_field_name("value"))
                else {
                    return false;
                };
                value.kind() == "self" && self.impl_type(receiver) == Some(*owner)
            }
            _ => self.constructed(receiver),
        }
    }

    /// The type an enclosing top-level `impl` is for, as written, without its generic
    /// arguments (`impl Source<'_>` is for `Source`). An `impl` inside a module has none.
    fn impl_type(&self, node: Node<'a>) -> Option<&'a str> {
        let mut current = node.parent();
        while let Some(ancestor) = current {
            if ancestor.kind() == "impl_item" {
                if !top_level(ancestor) {
                    return None;
                }
                let mut ty = ancestor.child_by_field_name("type")?;
                if ty.kind() == "generic_type" {
                    ty = ty.child_by_field_name("type")?;
                }
                return Some(self.text(ty).trim());
            }
            current = ancestor.parent();
        }
        None
    }

    /// Whether a name is bound exactly once in its enclosing function, before the use, by a
    /// parameter or `let` written with the client type. Any other binding of the name in the
    /// function (a rebinding, pattern, closure parameter, import or named item such as a
    /// block's `const`), or any glob import in the function, makes it ambiguous.
    fn bound_client(&self, name: Node<'a>) -> bool {
        let mut function = name.parent();
        while let Some(node) = function {
            if node.kind() == "function_item" {
                break;
            }
            function = node.parent();
        }
        let Some(function) = function else {
            return false;
        };
        let inside: Vec<_> = self
            .bindings
            .get(self.text(name))
            .into_iter()
            .flatten()
            .filter(|pattern| {
                function.start_byte() <= pattern.start_byte()
                    && pattern.end_byte() <= function.end_byte()
            })
            .collect();
        let glob = self
            .globs
            .iter()
            .any(|glob| function.start_byte() <= glob.start && glob.end <= function.end_byte());
        let [pattern] = inside[..] else {
            return false;
        };
        if glob {
            return false;
        }
        let Some(binding) = pattern
            .parent()
            .filter(|_| pattern.kind() == "identifier" && pattern.end_byte() <= name.start_byte())
        else {
            return false;
        };
        matches!(binding.kind(), "parameter" | "let_declaration")
            && binding
                .child_by_field_name("type")
                .is_some_and(|ty| self.client_type(ty))
    }

    /// Whether an expression yields a `reqwest` client by construction, with `Client` a
    /// client type ([`Self::client_type`]): `Client::new()`, or `Client::builder()`, any builder methods, `.build()` and then `?`,
    /// `.unwrap()` or `.expect(..)`; each optionally followed by `.clone()`. Any other method
    /// may change the type, so the chain is then not a client.
    fn constructed(&self, expression: Node<'a>) -> bool {
        // The steps from the receiver back to the constructor.
        let mut steps = Vec::new();
        let mut node = expression;
        let root = loop {
            match node.kind() {
                "call_expression" => {
                    let Some(function) = node.child_by_field_name("function") else {
                        return false;
                    };
                    match function.kind() {
                        "field_expression" => {
                            let (Some(field), Some(value)) = (
                                function.child_by_field_name("field"),
                                function.child_by_field_name("value"),
                            ) else {
                                return false;
                            };
                            steps.push(self.text(field));
                            node = value;
                        }
                        "scoped_identifier" => {
                            let (Some(path), Some(name)) = (
                                function.child_by_field_name("path"),
                                function.child_by_field_name("name"),
                            ) else {
                                return false;
                            };
                            if !self.client_type(path) {
                                return false;
                            }
                            break self.text(name);
                        }
                        _ => return false,
                    }
                }
                "try_expression" => {
                    steps.push("?");
                    match node.named_child(0) {
                        Some(inner) => node = inner,
                        None => return false,
                    }
                }
                "parenthesized_expression" => match node.named_child(0) {
                    Some(inner) => node = inner,
                    None => return false,
                },
                _ => return false,
            }
        };
        #[derive(PartialEq)]
        enum Made {
            Client,
            Builder,
            Built,
        }
        let mut made = match root {
            "new" => Made::Client,
            "builder" => Made::Builder,
            _ => return false,
        };
        for step in steps.into_iter().rev() {
            made = match (made, step) {
                (Made::Builder, "build") => Made::Built,
                (Made::Builder, step) if step != "?" => Made::Builder,
                (Made::Built, "?" | "unwrap" | "expect") => Made::Client,
                (Made::Client, "clone") => Made::Client,
                _ => return false,
            };
        }
        made == Made::Client
    }

    /// A string constant declared once at the top of the document, when the call is outside
    /// every inline module and so sees it. Anything else is not read.
    fn constant(&self, name: &str, visible: bool) -> Option<&str> {
        visible
            .then(|| self.constants.get(name))
            .flatten()
            .map(String::as_str)
    }

    /// The path template of a URL argument, with `{}` for each substitution. `None` when it
    /// cannot be read. `visible` says whether the call sees the top-level constants. Each
    /// substituted value is charged before it is copied.
    fn url(
        &self,
        node: Node<'a>,
        visible: bool,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        let mut node = node;
        while node.kind() == "reference_expression" {
            let Some(value) = node.child_by_field_name("value") else {
                return Ok(None);
            };
            node = value;
        }
        let text = match node.kind() {
            "string_literal" | "raw_string_literal" => self.literal(node),
            "identifier" => match self.constant(self.text(node), visible) {
                Some(text) => {
                    charge(work, text.len())?;
                    Some(text.to_owned())
                }
                None => None,
            },
            "macro_invocation" => self.format(node, visible, work, check)?,
            _ => None,
        };
        Ok(text.as_deref().and_then(path))
    }

    /// A `format!` call with substitutions read where they are string literals or visible
    /// constants without a format specification, and [`UNKNOWN`] elsewhere.
    fn format(
        &self,
        invocation: Node<'a>,
        visible: bool,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<String>> {
        let name: String = invocation
            .child_by_field_name("macro")
            .map(|name| self.text(name))
            .unwrap_or("")
            .split_whitespace()
            .collect();
        if !matches!(name.as_str(), "format" | "std::format" | "alloc::format") {
            return Ok(None);
        }
        let Some(tree) = Self::named_children(invocation)
            .into_iter()
            .find(|child| child.kind() == "token_tree")
        else {
            return Ok(None);
        };
        let mut cursor = tree.walk();
        let tokens: Vec<_> = tree
            .children(&mut cursor)
            .filter(|token| !comment(*token))
            .collect();
        let Some(inner) = tokens.get(1..tokens.len().saturating_sub(1)) else {
            return Ok(None);
        };
        let mut groups = inner.split(|token| token.kind() == ",");
        let Some([template]) = groups.next() else {
            return Ok(None);
        };
        let Some(template) = self.literal(*template) else {
            return Ok(None);
        };
        let mut positional = Vec::new();
        let mut named = BTreeMap::new();
        for group in groups {
            match group {
                [] => {}
                [name, equals, value] if name.kind() == "identifier" && equals.kind() == "=" => {
                    named.insert(self.text(*name), self.value(&[*value], visible));
                }
                value => positional.push(self.value(value, visible)),
            }
        }
        let mut out = String::new();
        let mut next = 0;
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.next_if_eq(&'{').is_some() => out.push('{'),
                '}' if chars.next_if_eq(&'}').is_some() => out.push('}'),
                '}' => return Ok(None),
                '{' => {
                    check()?;
                    let mut spec = String::new();
                    loop {
                        match chars.next() {
                            None => return Ok(None),
                            Some('}') => break,
                            Some(c) => spec.push(c),
                        }
                    }
                    let (argument, format) = spec.split_once(':').unwrap_or((&spec, ""));
                    let argument = argument.trim();
                    let value = if argument.is_empty() {
                        next += 1;
                        positional.get(next - 1).cloned().flatten()
                    } else if let Ok(index) = argument.parse::<usize>() {
                        positional.get(index).cloned().flatten()
                    } else {
                        match named.get(&argument) {
                            Some(value) => value.clone(),
                            None => self.constant(argument, visible).map(Cow::Borrowed),
                        }
                    };
                    // A fill, width, precision or other mode changes the text, so the value
                    // is a substitution, never its raw text.
                    match value.filter(|_| format.is_empty()) {
                        Some(text) => {
                            charge(work, text.len())?;
                            out.push_str(&text);
                        }
                        None => out.push(UNKNOWN),
                    }
                }
                c => out.push(c),
            }
        }
        Ok(Some(out))
    }

    /// A format argument that is one string literal or visible constant.
    fn value(&self, tokens: &[Node<'a>], visible: bool) -> Option<Cow<'_, str>> {
        match tokens {
            [token] if token.kind() == "identifier" => {
                self.constant(self.text(*token), visible).map(Cow::Borrowed)
            }
            [token] => self.literal(*token).map(Cow::Owned),
            _ => None,
        }
    }
}

/// Whether an item is at the top level of its document, outside every inline module.
fn top_level(item: Node<'_>) -> bool {
    item.parent()
        .is_some_and(|parent| parent.kind() == "source_file")
}

/// The path of an absolute URL or a rooted path, cut at `?` or `#`, with `{}` for each
/// substitution. A URL that starts with a substitution is unreadable, since that may carry
/// its own path.
pub(super) fn path(url: &str) -> Option<String> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let rest = if lower.starts_with("http://") {
        &url[7..]
    } else if lower.starts_with("https://") {
        &url[8..]
    } else {
        ""
    };
    let path = if lower.starts_with("http://") || lower.starts_with("https://") {
        match rest.find(['/', '?', '#']) {
            Some(at) if rest[at..].starts_with('/') => &rest[at..],
            Some(_) => "/",
            None if rest.contains(UNKNOWN) => return None,
            None => "/",
        }
    } else if url.starts_with('/') {
        url
    } else {
        return None;
    };
    let end = path.find(['?', '#']).unwrap_or(path.len());
    Some(path[..end].replace(UNKNOWN, "{}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_read_as_their_path() {
        let unknown = UNKNOWN.to_string();
        for (url, read) in [
            (
                "http://localhost:8080/api/users".to_owned(),
                Some("/api/users"),
            ),
            ("HTTPS://example.test".to_owned(), Some("/")),
            ("https://example.test?q=1".to_owned(), Some("/")),
            ("/users/1?page=2#top".to_owned(), Some("/users/1")),
            (
                format!("http://{unknown}:{unknown}/users/{unknown}"),
                Some("/users/{}"),
            ),
            (format!("http://{unknown}"), None),
            (format!("{unknown}/users"), None),
            ("users".to_owned(), None),
            ("ftp://example.test/x".to_owned(), None),
        ] {
            assert_eq!(path(&url).as_deref(), read, "{url:?}");
        }
    }
}
