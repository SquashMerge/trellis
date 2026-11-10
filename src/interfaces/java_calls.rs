//! Java client calls (#857): `java.net.http` requests built with `HttpRequest.newBuilder()`,
//! Spring `RestTemplate` calls and Spring `WebClient` requests. A URL is read where it is a
//! string expression `java` can read; anything else is an unreadable path.
use super::calls::client_segments;
use super::java::{Document, Views, unparenthesize};
use super::routes::join;
use super::rust_calls::{UNKNOWN, path};
use super::{ClientCall, ClientPrefix, charge, test_span};
use crate::references::{callers::Callers, walk, work::Work};
use anyhow::Result;
use std::collections::HashMap;
use tree_sitter::Node;

const HTTP: &str = "java.net.http";
const NET: &str = "java.net";
const SPRING_CLIENT: &str = "org.springframework.web.client";
const REACTIVE: &str = "org.springframework.web.reactive.function.client";
const SPRING_HTTP: &str = "org.springframework.http";
const BOOT_CLIENT: [&str; 2] = [
    "org.springframework.boot.web.client",
    "org.springframework.boot.restclient",
];
const VERBS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
/// The `RestTemplate` methods that take the URL first, and the method each sends.
const REST_TEMPLATE: [(&str, &str); 10] = [
    ("getForObject", "GET"),
    ("getForEntity", "GET"),
    ("headForHeaders", "HEAD"),
    ("postForObject", "POST"),
    ("postForEntity", "POST"),
    ("postForLocation", "POST"),
    ("put", "PUT"),
    ("patchForObject", "PATCH"),
    ("delete", "DELETE"),
    ("optionsForAllow", "OPTIONS"),
];

/// Which client a receiver is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    RestTemplate,
    WebClient,
}

/// A client's base URL path.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Base {
    /// The client has none: a path is as written.
    None,
    /// The path of a readable base URL.
    Known(String),
    /// The client may have one the document does not show.
    Unknown,
}

/// The client calls of every client candidate, in document then source order.
pub(super) fn extract(
    clients: &[usize],
    views: &Views<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<ClientCall>> {
    let mut found = Vec::new();
    for index in clients {
        check()?;
        let Some(view) = views.get(index) else {
            continue;
        };
        charge(work, view.source.len())?;
        let mut sites = Vec::new();
        walk(view.root, check, &mut |node, _| {
            if node.kind() == "method_invocation" {
                sites.push(node);
            }
            Ok(false)
        })?;
        let mut reader = Reader {
            view,
            receivers: HashMap::new(),
        };
        let callers = Callers::new(view.doc);
        let mut calls = Vec::new();
        for node in sites {
            check()?;
            let range = Document::range(node);
            if node.has_error() || test_span(view.doc, &range) {
                continue;
            }
            let Some((method, path, prefix)) = reader.call(node, work, check)? else {
                continue;
            };
            let call_id = view
                .doc
                .calls
                .iter()
                .find(|call| call.range == range)
                .map(|call| call.id.clone());
            calls.push(ClientCall {
                method: method.map(str::to_owned),
                segments: path.as_deref().map(client_segments),
                path_template: path,
                prefix,
                document: view.doc.key.clone(),
                lines: view.lines.lines(&range),
                caller: callers.get(call_id.as_deref(), &range).0,
                call_id,
                range,
            });
        }
        calls.sort_by_key(|call: &ClientCall| (call.range.start, call.range.end));
        found.extend(calls);
    }
    Ok(found)
}

type Read = (Option<&'static str>, Option<String>, ClientPrefix);

struct Reader<'v, 'a> {
    view: &'v Document<'a>,
    /// What each receiver name reads as, once per name.
    receivers: HashMap<&'a str, Option<(Kind, Base)>>,
}

impl<'a> Reader<'_, 'a> {
    fn text(&self, node: Node<'_>) -> &'a str {
        self.view.text(node)
    }

    /// One client call: its method, path and how the path was assembled. `None` when the
    /// invocation is not a client call.
    fn call(
        &mut self,
        node: Node<'a>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Read>> {
        let Some(name) = node.child_by_field_name("name").map(|n| self.text(n)) else {
            return Ok(None);
        };
        let arguments = Document::arguments(node);
        if name == "build" && arguments.is_empty() {
            return self.http_request(node, work, check);
        }
        let Some(object) = node.child_by_field_name("object") else {
            return Ok(None);
        };
        let rest = REST_TEMPLATE.iter().find(|(method, _)| *method == name);
        let web = VERBS
            .iter()
            .copied()
            .find(|verb| verb.eq_ignore_ascii_case(name) && name.chars().all(char::is_lowercase));
        if rest.is_none() && web.is_none() && !matches!(name, "exchange" | "execute" | "method") {
            return Ok(None);
        }
        let Some((kind, base)) = self.receiver(object, work, check)? else {
            return Ok(None);
        };
        match kind {
            Kind::RestTemplate => {
                let method = match (rest, name) {
                    (Some((_, method)), _) => Some(*method),
                    (None, "exchange" | "execute") => match arguments.get(1) {
                        Some(method) => self.http_method(*method),
                        None => None,
                    },
                    _ => return Ok(None),
                };
                let Some(url) = arguments.first() else {
                    return Ok(None);
                };
                let (path, prefix) = self.spring_url(*url, &base, work, check)?;
                Ok(Some((method, path, prefix)))
            }
            Kind::WebClient => {
                let method = match (web, name, &arguments[..]) {
                    (Some(verb), _, []) => Some(verb),
                    (None, "method", [method]) => self.http_method(*method),
                    _ => return Ok(None),
                };
                // `uri(..)` is called on what the method selector returns.
                let uri = node.parent().filter(|parent| {
                    parent.kind() == "method_invocation"
                        && parent.child_by_field_name("object") == Some(node)
                        && parent
                            .child_by_field_name("name")
                            .is_some_and(|n| self.text(n) == "uri")
                });
                let url = uri.and_then(|uri| Document::arguments(uri).first().copied());
                let (path, prefix) = match url {
                    Some(url) => self.spring_url(url, &base, work, check)?,
                    None => (None, ClientPrefix::Written),
                };
                Ok(Some((method, path, prefix)))
            }
        }
    }

    /// `HttpRequest.newBuilder(..)...build()`: the last method set (`GET` when none) and the
    /// last URI set, each through `URI.create(s)` or `new URI(s)`.
    fn http_request(
        &self,
        build: Node<'a>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<Read>> {
        let mut method: Option<Option<&'static str>> = None;
        let mut uri: Option<Option<Node<'a>>> = None;
        let mut node = build.child_by_field_name("object");
        loop {
            check()?;
            charge(work, 1)?;
            let Some(link) = node.filter(|n| n.kind() == "method_invocation") else {
                return Ok(None);
            };
            let name = link.child_by_field_name("name").map(|n| self.text(n));
            let arguments = Document::arguments(link);
            let object = link.child_by_field_name("object");
            if name == Some("newBuilder")
                && object.is_some_and(|object| {
                    self.view
                        .resolves(&self.view.compact(object), &[HTTP], "HttpRequest")
                })
            {
                if uri.is_none() {
                    uri = Some(match &arguments[..] {
                        [only] => Some(*only),
                        _ => None,
                    });
                }
                break;
            }
            match (name, &arguments[..]) {
                (Some("GET" | "DELETE" | "HEAD"), []) | (Some("POST" | "PUT"), [_]) => {
                    method
                        .get_or_insert(name.and_then(|n| VERBS.iter().copied().find(|v| *v == n)));
                }
                (Some("method"), [verb, _]) => {
                    let verb = self
                        .view
                        .literal(*verb)
                        .and_then(|text| VERBS.iter().copied().find(|v| *v == text.as_ref()));
                    method.get_or_insert(verb);
                }
                (Some("uri"), [only]) => {
                    uri.get_or_insert(Some(*only));
                }
                _ => {}
            }
            node = object;
        }
        let method = method.unwrap_or(Some("GET"));
        let Some(Some(uri)) = uri else {
            return Ok(Some((method, None, ClientPrefix::Written)));
        };
        let text = match self.uri_argument(uri) {
            Some(text) => self.view.string(text, work, check)?,
            None => UNKNOWN.to_string(),
        };
        let (path, prefix) = assemble(&text, &Base::None);
        Ok(Some((method, path, prefix)))
    }

    /// The string inside `URI.create(s)` or `new URI(s)`.
    fn uri_argument(&self, node: Node<'a>) -> Option<Node<'a>> {
        let node = unparenthesize(node);
        let arguments = Document::arguments(node);
        let [only] = arguments[..] else {
            return None;
        };
        let named = match node.kind() {
            "method_invocation" => {
                node.child_by_field_name("name").map(|n| self.text(n)) == Some("create")
                    && node.child_by_field_name("object").is_some_and(|object| {
                        self.view
                            .resolves(&self.view.compact(object), &[NET], "URI")
                    })
            }
            "object_creation_expression" => {
                node.child_by_field_name("type")
                    .is_some_and(|ty| self.view.resolves(&self.view.compact(ty), &[NET], "URI"))
                    && !has_body(node)
            }
            _ => false,
        };
        named.then_some(only)
    }

    /// A Spring client URL: a string, or a `java.net.URI` of one. Each `{name}` URI template
    /// variable is a substitution.
    fn spring_url(
        &self,
        url: Node<'a>,
        base: &Base,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<(Option<String>, ClientPrefix)> {
        let node = self.uri_argument(url).unwrap_or(url);
        let text = self.view.string(node, work, check)?;
        Ok(assemble(&variables(&text), base))
    }

    /// `HttpMethod.M`.
    fn http_method(&self, node: Node<'a>) -> Option<&'static str> {
        let node = unparenthesize(node);
        if node.kind() != "field_access" {
            return None;
        }
        let object = self.view.compact(node.child_by_field_name("object")?);
        let field = self.text(node.child_by_field_name("field")?);
        if !self.view.resolves(&object, &[SPRING_HTTP], "HttpMethod") {
            return None;
        }
        VERBS.iter().copied().find(|verb| *verb == field)
    }

    /// What a call's receiver reads as: a `RestTemplate` or `WebClient`, and its base.
    fn receiver(
        &mut self,
        object: Node<'a>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<(Kind, Base)>> {
        let object = unparenthesize(object);
        let name = match object.kind() {
            "identifier" => Some(self.text(object)),
            "field_access"
                if object
                    .child_by_field_name("object")
                    .is_some_and(|o| o.kind() == "this") =>
            {
                object.child_by_field_name("field").map(|f| self.text(f))
            }
            _ => None,
        };
        let Some(name) = name else {
            return self.expression(object, work, check);
        };
        if let Some(known) = self.receivers.get(name) {
            return Ok(known.clone());
        }
        let read = self.named(name, work, check)?;
        self.receivers.insert(name, read.clone());
        Ok(read)
    }

    /// A receiver name: every declaration of it in the document must type it as the same
    /// client. Its base is the one every value the document gives it agrees on; a
    /// parameter, a declaration with no value and nothing assigned, or any value that does
    /// not show its base, leaves the base unknown.
    fn named(
        &self,
        name: &str,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<(Kind, Base)>> {
        let Some(bindings) = self.view.declared.get(name) else {
            return Ok(None);
        };
        let mut kind = None;
        let mut bases = Vec::new();
        let mut unknown = false;
        for binding in bindings {
            check()?;
            let declared = match binding.ty {
                Some(ty) if self.text(ty) == "var" => None,
                Some(ty) => match self.kind_of_type(ty) {
                    Some(kind) => Some(kind),
                    None => return Ok(None),
                },
                None => return Ok(None),
            };
            let from_value = match binding.value {
                Some(value) => self.expression(value, work, check)?,
                None => None,
            };
            let this = match (declared, &from_value) {
                (Some(kind), _) => kind,
                (None, Some((kind, _))) => *kind,
                (None, None) => return Ok(None),
            };
            if kind.is_some_and(|kind| kind != this) {
                return Ok(None);
            }
            kind = Some(this);
            if binding.parameter {
                unknown = true;
            }
            if binding.value.is_some() {
                match from_value {
                    Some((_, base)) => bases.push(base),
                    None => unknown = true,
                }
            }
        }
        let Some(kind) = kind else {
            return Ok(None);
        };
        for value in self.view.assigned.get(name).into_iter().flatten() {
            check()?;
            match self.expression(*value, work, check)? {
                Some((assigned, base)) if assigned == kind => bases.push(base),
                _ => unknown = true,
            }
        }
        let base = match bases.split_first() {
            Some((first, rest)) if !unknown && rest.iter().all(|base| base == first) => {
                first.clone()
            }
            _ => Base::Unknown,
        };
        Ok(Some((kind, base)))
    }

    /// A declared type that is a client: `RestTemplate`, `RestOperations` or `WebClient`.
    fn kind_of_type(&self, ty: Node<'_>) -> Option<Kind> {
        let written = self.view.compact(ty);
        if self
            .view
            .resolves(&written, &[SPRING_CLIENT], "RestTemplate")
            || self
                .view
                .resolves(&written, &[SPRING_CLIENT], "RestOperations")
        {
            return Some(Kind::RestTemplate);
        }
        self.view
            .resolves(&written, &[REACTIVE], "WebClient")
            .then_some(Kind::WebClient)
    }

    /// An expression that builds a client: `new RestTemplate(..)`, `WebClient.create(..)`,
    /// or a `RestTemplateBuilder` or `WebClient.builder()` chain ending in `.build()`, whose
    /// last `rootUri(s)` or `baseUrl(s)` is the base.
    fn expression(
        &self,
        node: Node<'a>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Option<(Kind, Base)>> {
        let node = unparenthesize(node);
        if node.kind() == "object_creation_expression" {
            let rest = node.child_by_field_name("type").is_some_and(|ty| {
                self.view
                    .resolves(&self.view.compact(ty), &[SPRING_CLIENT], "RestTemplate")
            });
            return Ok((rest && !has_body(node)).then_some((Kind::RestTemplate, Base::None)));
        }
        if node.kind() != "method_invocation" {
            return Ok(None);
        }
        let name = |link: Node<'a>| link.child_by_field_name("name").map(|n| self.text(n));
        let webclient = |object: Option<Node<'a>>| {
            object.is_some_and(|object| {
                self.view
                    .resolves(&self.view.compact(object), &[REACTIVE], "WebClient")
            })
        };
        let arguments = Document::arguments(node);
        if name(node) == Some("create") && webclient(node.child_by_field_name("object")) {
            return Ok(match &arguments[..] {
                [] => Some((Kind::WebClient, Base::None)),
                [url] => Some((Kind::WebClient, self.base(*url, work, check)?)),
                _ => None,
            });
        }
        if name(node) != Some("build") || !arguments.is_empty() {
            return Ok(None);
        }
        // Links from the outermost in: the first base link seen is the last one set.
        let mut base: Option<Base> = None;
        let mut link = node.child_by_field_name("object");
        loop {
            check()?;
            charge(work, 1)?;
            let Some(current) = link.map(unparenthesize) else {
                return Ok(None);
            };
            match current.kind() {
                "object_creation_expression" => {
                    let builder = current.child_by_field_name("type").is_some_and(|ty| {
                        BOOT_CLIENT.iter().any(|package| {
                            self.view.resolves(
                                &self.view.compact(ty),
                                &[*package],
                                "RestTemplateBuilder",
                            )
                        })
                    });
                    if !builder || has_body(current) {
                        return Ok(None);
                    }
                    return Ok(Some((Kind::RestTemplate, base.unwrap_or(Base::None))));
                }
                "method_invocation" => {
                    let arguments = Document::arguments(current);
                    let object = current.child_by_field_name("object");
                    match (name(current), &arguments[..]) {
                        (Some("builder"), []) if webclient(object) => {
                            return Ok(Some((Kind::WebClient, base.unwrap_or(Base::None))));
                        }
                        (Some("rootUri" | "baseUrl"), [url]) => {
                            if base.is_none() {
                                base = Some(self.base(*url, work, check)?);
                            }
                        }
                        (Some("uriTemplateHandler" | "uriBuilderFactory"), _) => {
                            base.get_or_insert(Base::Unknown);
                        }
                        _ => {}
                    }
                    link = object;
                }
                "identifier" => {
                    // An injected builder may already carry a base.
                    let kind = self.builder_kind(self.text(current));
                    return Ok(kind.map(|kind| (kind, base.unwrap_or(Base::Unknown))));
                }
                _ => return Ok(None),
            }
        }
    }

    /// The client a builder variable builds, when every declaration types it as one builder.
    fn builder_kind(&self, name: &str) -> Option<Kind> {
        let bindings = self.view.declared.get(name)?;
        let mut kind = None;
        for binding in bindings {
            let written = self.view.compact(binding.ty?);
            let this = if BOOT_CLIENT.iter().any(|package| {
                self.view
                    .resolves(&written, &[*package], "RestTemplateBuilder")
            }) {
                Kind::RestTemplate
            } else if written
                .strip_suffix(".Builder")
                .is_some_and(|client| self.view.resolves(client, &[REACTIVE], "WebClient"))
            {
                Kind::WebClient
            } else {
                return None;
            };
            if kind.is_some_and(|kind| kind != this) {
                return None;
            }
            kind = Some(this);
        }
        kind
    }

    /// The path of a base URL. One that cannot be read completely is unknown.
    fn base(
        &self,
        url: Node<'a>,
        work: &mut Work,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<Base> {
        let text = variables(&self.view.string(url, work, check)?);
        Ok(match path(&text) {
            Some(path) if !path.contains("{}") => Base::Known(path),
            _ => Base::Unknown,
        })
    }
}

fn has_body(creation: Node<'_>) -> bool {
    let mut cursor = creation.walk();
    creation
        .named_children(&mut cursor)
        .any(|child| child.kind() == "class_body")
}

/// Each `{name}` URI template variable as a substitution. An unclosed `{` makes the rest
/// unreadable.
fn variables(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '{' {
            out.push(c);
            continue;
        }
        out.push(UNKNOWN);
        if !chars.by_ref().any(|c| c == '}') {
            break;
        }
    }
    out
}

/// The path a URL reaches from a client with `base`. An `http` or `https` URL is its own
/// path; a rooted path is under the base's path, and unreadable when the base is unknown.
fn assemble(url: &str, base: &Base) -> (Option<String>, ClientPrefix) {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") || !url.starts_with('/') {
        return (path(url), ClientPrefix::Written);
    }
    match base {
        Base::None => (path(url), ClientPrefix::Written),
        Base::Known(prefix) if prefix.trim_matches('/').is_empty() => {
            (path(url), ClientPrefix::Written)
        }
        Base::Known(prefix) => (path(&join(prefix, url)), ClientPrefix::Joined),
        Base::Unknown => (None, ClientPrefix::Written),
    }
}
