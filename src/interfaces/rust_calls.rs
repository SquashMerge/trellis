//! Rust client calls (#858): `reqwest` requests whose URL is a literal, a `format!` with a
//! readable path, or a same-document string constant. Anything else is an unreadable path.
use super::calls::{client_segments, verb};
use super::{ClientCall, LineIndex, comment, routes::path_verb, test_span, under_test_support};
use crate::native_evidence::Range;
use crate::references::{DocumentInput, callers::Callers, walk};
use anyhow::Result;
use std::collections::BTreeMap;
use tree_sitter::Node;

/// The request builders of a `reqwest` client that take only the URL.
const BUILDERS: [&str; 6] = ["get", "post", "put", "delete", "patch", "head"];
/// The free functions that send a `GET`.
const FREE: [&str; 2] = ["reqwest::get", "reqwest::blocking::get"];
/// Stands for a substitution while a URL is assembled.
const UNKNOWN: char = '\0';

/// Whether a Rust document can make `reqwest` calls.
pub(super) fn candidate(doc: &DocumentInput) -> bool {
    doc.key.language == "rust" && doc.file.text.contains("reqwest")
}

/// The `reqwest` calls of one document, in source order.
pub(super) fn collect(
    doc: &DocumentInput,
    root: Node<'_>,
    found: &mut Vec<ClientCall>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    let document = Document {
        source: &doc.file.text,
        constants: BTreeMap::new(),
    };
    let mut constants: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut sites = Vec::new();
    walk(root, check, &mut |node, _| {
        match node.kind() {
            "const_item" | "static_item" => {
                let name = node.child_by_field_name("name").map(|n| document.text(n));
                let value = node
                    .child_by_field_name("value")
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
    let document = Document {
        constants: constants
            .into_iter()
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
        let template = document.url(url);
        let call_id = doc
            .calls
            .iter()
            .find(|call| call.range == range)
            .map(|call| call.id.clone());
        calls.push(ClientCall {
            wrapper: false,
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

    /// Whether a receiver reads as a `reqwest` client: a name or field containing `client`
    /// (any case), or a `Client::new()` or `Client::builder()` chain.
    fn client(&self, receiver: Node<'a>) -> bool {
        let named = |node: Node<'_>| self.text(node).to_ascii_lowercase().contains("client");
        match receiver.kind() {
            "identifier" => named(receiver),
            "field_expression" => receiver.child_by_field_name("field").is_some_and(named),
            "call_expression" | "try_expression" | "await_expression" => {
                let text: String = self.text(receiver).split_whitespace().collect();
                text.contains("Client::new()") || text.contains("Client::builder()")
            }
            _ => false,
        }
    }

    /// The path template of a URL argument, with `{}` for each substitution. `None` when it
    /// cannot be read.
    fn url(&self, node: Node<'a>) -> Option<String> {
        let mut node = node;
        while node.kind() == "reference_expression" {
            node = node.child_by_field_name("value")?;
        }
        let text = match node.kind() {
            "string_literal" | "raw_string_literal" => self.literal(node)?,
            "identifier" => self.constants.get(self.text(node))?.clone(),
            "macro_invocation" => self.format(node)?,
            _ => return None,
        };
        path(&text)
    }

    /// A `format!` call with substitutions read where they are string literals or constants,
    /// and [`UNKNOWN`] elsewhere.
    fn format(&self, invocation: Node<'a>) -> Option<String> {
        let name: String = self
            .text(invocation.child_by_field_name("macro")?)
            .split_whitespace()
            .collect();
        if !matches!(name.as_str(), "format" | "std::format" | "alloc::format") {
            return None;
        }
        let tree = Self::named_children(invocation)
            .into_iter()
            .find(|child| child.kind() == "token_tree")?;
        let mut cursor = tree.walk();
        let tokens: Vec<_> = tree
            .children(&mut cursor)
            .filter(|token| !comment(*token))
            .collect();
        let inner = tokens.get(1..tokens.len().checked_sub(1)?)?;
        let mut groups = inner.split(|token| token.kind() == ",");
        let [template] = groups.next()? else {
            return None;
        };
        let template = self.literal(*template)?;
        let mut positional = Vec::new();
        let mut named = BTreeMap::new();
        for group in groups {
            match group {
                [] => {}
                [name, equals, value] if name.kind() == "identifier" && equals.kind() == "=" => {
                    named.insert(self.text(*name), self.value(&[*value]));
                }
                value => positional.push(self.value(value)),
            }
        }
        let mut out = String::new();
        let mut next = 0;
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.next_if_eq(&'{').is_some() => out.push('{'),
                '}' if chars.next_if_eq(&'}').is_some() => out.push('}'),
                '}' => return None,
                '{' => {
                    let mut spec = String::new();
                    loop {
                        match chars.next()? {
                            '}' => break,
                            c => spec.push(c),
                        }
                    }
                    let argument = spec.split(':').next().unwrap_or("").trim();
                    let value = if argument.is_empty() {
                        next += 1;
                        positional.get(next - 1).cloned().flatten()
                    } else if let Ok(index) = argument.parse::<usize>() {
                        positional.get(index).cloned().flatten()
                    } else {
                        match named.get(&argument) {
                            Some(value) => value.clone(),
                            None => self.constants.get(argument).cloned(),
                        }
                    };
                    match value {
                        Some(text) => out.push_str(&text),
                        None => out.push(UNKNOWN),
                    }
                }
                c => out.push(c),
            }
        }
        Some(out)
    }

    /// A format argument that is one string literal or constant.
    fn value(&self, tokens: &[Node<'a>]) -> Option<String> {
        match tokens {
            [token] if token.kind() == "identifier" => {
                self.constants.get(self.text(*token)).cloned()
            }
            [token] => self.literal(*token),
            _ => None,
        }
    }
}

/// The path of an absolute URL or a rooted path, cut at `?` or `#`, with `{}` for each
/// substitution. A URL that starts with a substitution is unreadable, since that may carry
/// its own path.
fn path(url: &str) -> Option<String> {
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
