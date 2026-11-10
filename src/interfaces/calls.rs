//! Client calls: JavaScript and TypeScript request wrappers, `fetch` and axios (#788, #855),
//! and the matching of each call to the server route it reaches.
use super::js::{Base, Budget, Value, World, arguments, spread, unwrap, verb};
use super::{
    ANY, ClientCall, ClientPrefix, Confidence, LineIndex, Match, Route, Seg, charge, test_file,
    test_span,
};
use crate::native_evidence::{Call, Range};
use crate::references::{DocumentInput, callers::Callers, work::Work};
use anyhow::Result;
use tree_sitter::Node;

/// The receivers of `<owner>.request(path, options)`.
const OWNERS: [&str; 2] = ["api", "owner"];
/// The objects whose `fetch` is the global one.
const GLOBALS: [&str; 3] = ["window", "globalThis", "self"];
/// Axios request methods and the argument their config is at; `request` has no URL argument.
const AXIOS_METHODS: [(&str, usize); 11] = [
    ("request", 0),
    ("get", 1),
    ("delete", 1),
    ("head", 1),
    ("options", 1),
    ("post", 2),
    ("put", 2),
    ("patch", 2),
    ("postForm", 2),
    ("putForm", 2),
    ("patchForm", 2),
];

/// Where a wrapper reads its method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MethodAt {
    /// A string literal at this argument; absent is `GET`.
    Argument(usize),
    /// `method` in the options object at this argument; absent is `GET`.
    Options(usize),
}

/// A request wrapper: a callee whose argument 0 is the path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Wrapper {
    /// An identifier, or `object.property`.
    pub(crate) callee: String,
    pub(crate) method: MethodAt,
    /// A configured prefix the wrapper adds to every path, or the built-in checkout prefix
    /// rule when `None` for a built-in.
    pub(crate) prefix: Option<String>,
    pub(crate) built_in: bool,
}

/// The built-in wrappers of #788.
pub(crate) fn built_in() -> Vec<Wrapper> {
    let wrapper = |callee: &str, method| Wrapper {
        callee: callee.to_owned(),
        method,
        prefix: None,
        built_in: true,
    };
    vec![
        wrapper("api", MethodAt::Argument(1)),
        wrapper("request", MethodAt::Options(2)),
        wrapper("api.request", MethodAt::Options(1)),
        wrapper("owner.request", MethodAt::Options(1)),
    ]
}

/// Configured wrappers that apply under one directory.
pub(crate) struct Scoped {
    /// A checkout-relative directory; empty is the checkout root.
    pub(crate) root: String,
    pub(crate) wrappers: Vec<Wrapper>,
}

/// What a client reads its call as.
enum Form {
    Wrapper(Wrapper),
    Fetch,
    /// An axios call: its base URL, the argument its URL is at (if any) and its config's.
    Axios {
        base: Base,
        url: Option<usize>,
        config: usize,
        method: Option<&'static str>,
    },
}

/// What a client's path reads as.
enum Path {
    Unreadable,
    /// A readable path that is not rooted, or an absolute URL: not a client call.
    Foreign,
    Read(String, ClientPrefix),
}

/// The client calls of every production JavaScript and TypeScript document.
pub(super) fn extract(
    world: &World<'_>,
    configured: &[Scoped],
    budget: &mut Budget<'_>,
) -> Result<Vec<ClientCall>> {
    let mut found = Vec::new();
    // An axios client's methods are candidates only where some document can make one.
    let mut axios_anywhere = false;
    for doc in &world.snapshot.documents {
        budget.step(1)?;
        axios_anywhere |= super::js::javascript(doc) && doc.file.text.contains("axios");
    }
    for at in 0..world.snapshot.documents.len() {
        budget.step(1)?;
        let doc = world.doc(at);
        if !world.readable(at) || test_file(doc) {
            continue;
        }
        let mut wrappers: Vec<Wrapper> = configured
            .iter()
            .filter(|scope| under(&doc.key.path, &scope.root))
            .flat_map(|scope| scope.wrappers.iter().cloned())
            .collect();
        for wrapper in built_in() {
            if !wrappers.iter().any(|w| w.callee == wrapper.callee) {
                wrappers.push(wrapper);
            }
        }
        let axios = Axios {
            here: doc.file.text.contains("axios"),
            anywhere: axios_anywhere,
        };
        if !doc
            .calls
            .iter()
            .any(|call| candidate(doc, call, &wrappers, axios))
        {
            continue;
        }
        let Some(tree) = world.tree(at, budget)? else {
            continue;
        };
        Document {
            world,
            at,
            doc,
            root: tree.root_node(),
            wrappers: &wrappers,
            axios,
        }
        .collect(&mut found, budget)?;
    }
    Ok(found)
}

/// Whether a path lies under a directory.
fn under(path: &str, root: &str) -> bool {
    root.is_empty()
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn callee_text<'d>(doc: &'d DocumentInput, call: &'d Call) -> &'d str {
    call.callee_range
        .as_ref()
        .and_then(|range| doc.file.text.get(range.start..range.end))
        .or(call.spelling.as_deref())
        .unwrap_or("")
}

/// Whether axios is mentioned in this document, and in any document.
#[derive(Clone, Copy)]
struct Axios {
    here: bool,
    anywhere: bool,
}

/// Whether a native call may be a client call, by the last name of its callee.
fn candidate(doc: &DocumentInput, call: &Call, wrappers: &[Wrapper], axios: Axios) -> bool {
    let text = callee_text(doc, call);
    let last = text
        .rsplit(['.', ' ', '\n', '\t', '\r'])
        .next()
        .unwrap_or(text);
    last == "fetch"
        || (axios.here && !text.contains('.'))
        || (axios.anywhere && AXIOS_METHODS.iter().any(|(name, _)| *name == last))
        || wrappers
            .iter()
            .any(|w| w.callee.rsplit('.').next() == Some(last))
}

struct Document<'w, 's> {
    world: &'w World<'s>,
    at: usize,
    doc: &'s DocumentInput,
    root: Node<'w>,
    wrappers: &'w [Wrapper],
    axios: Axios,
}

impl<'w> Document<'w, '_> {
    fn text(&self, node: Node<'_>) -> &str {
        self.world.text(self.at, node)
    }

    fn collect(&self, found: &mut Vec<ClientCall>, budget: &mut Budget<'_>) -> Result<()> {
        let lines = LineIndex::new(&self.doc.file.text);
        let callers = Callers::new(self.doc);
        let mut sites: Vec<(&Call, Node<'w>, Form)> = Vec::new();
        for call in &self.doc.calls {
            budget.step(1)?;
            if !candidate(self.doc, call, self.wrappers, self.axios)
                || test_span(self.doc, &call.range)
            {
                continue;
            }
            let Some(node) = self.call_node(&call.range).filter(|node| !node.has_error()) else {
                continue;
            };
            let Some(function) = node.child_by_field_name("function") else {
                continue;
            };
            if let Some(form) = self.form(function, budget)? {
                if !matches!(form, Form::Wrapper(_)) && self.inside_wrapper(&call.range) {
                    continue;
                }
                sites.push((call, node, form));
            }
        }
        sites.sort_by_key(|(call, ..)| (call.range.start, call.range.end));
        for (call, node, form) in sites {
            let arguments = arguments(node);
            let (path, method) = self.read(&form, &arguments, budget)?;
            let (template, segments, prefix) = match path {
                Path::Foreign => continue,
                Path::Unreadable => (None, None, ClientPrefix::Written),
                Path::Read(template, prefix) => {
                    let segments = client_segments(&template);
                    (Some(template), Some(segments), prefix)
                }
            };
            found.push(ClientCall {
                method: method.map(str::to_owned),
                path_template: template,
                segments,
                prefix,
                document: self.doc.key.clone(),
                lines: lines.lines(&call.range),
                range: call.range.clone(),
                call_id: Some(call.id.clone()),
                caller: callers.get(Some(&call.id), &call.range).0,
            });
        }
        Ok(())
    }

    /// Whether the span lies inside a named callable that is itself a wrapper: its own
    /// `fetch` or axios call is the wrapper's implementation, not a call of its own.
    fn inside_wrapper(&self, span: &Range) -> bool {
        self.doc.declarations.iter().any(|d| {
            crate::metrics::callable(&d.kind)
                && d.range.start <= span.start
                && span.end <= d.range.end
                && d.name.as_deref().is_some_and(|name| {
                    self.wrappers
                        .iter()
                        .any(|w| w.callee.rsplit('.').next() == Some(name))
                })
        })
    }

    /// The call expression whose span is exactly `range`. A native call over an `await` spans
    /// the awaited expression, so that one reaches the call it awaits.
    fn call_node(&self, range: &Range) -> Option<Node<'w>> {
        let mut node = self
            .root
            .descendant_for_byte_range(range.start, range.end)?;
        loop {
            if node.start_byte() == range.start && node.end_byte() == range.end {
                match node.kind() {
                    "call_expression" => return Some(node),
                    "await_expression" => {
                        return node
                            .named_child(0)
                            .filter(|inner| inner.kind() == "call_expression");
                    }
                    _ => {}
                }
            }
            node = node.parent()?;
            if node.start_byte() > range.start || node.end_byte() < range.end {
                return None;
            }
        }
    }

    /// What the callee makes the call: a wrapper, `fetch`, an axios call, or none.
    fn form(&self, function: Node<'w>, budget: &mut Budget<'_>) -> Result<Option<Form>> {
        // The TypeScript grammar reads `await api<T>(path)` as a call of `await api`.
        let function = match function.kind() {
            "await_expression" => match function.named_child(0) {
                Some(inner) => inner,
                None => return Ok(None),
            },
            _ => function,
        };
        let written: String = self.text(function).split_whitespace().collect();
        let simple = match function.kind() {
            "identifier" => true,
            "member_expression" => function
                .child_by_field_name("object")
                .is_some_and(|object| object.kind() == "identifier"),
            _ => false,
        };
        if simple && let Some(wrapper) = self.wrappers.iter().find(|w| w.callee == written) {
            // A built-in `x.request` is a wrapper only for its known owners.
            let owned = !wrapper.built_in
                || !written.contains('.')
                || OWNERS.contains(&written.split('.').next().unwrap_or(""));
            if owned {
                return Ok(Some(Form::Wrapper(wrapper.clone())));
            }
        }
        if written == "fetch"
            || written
                .split_once('.')
                .is_some_and(|(object, property)| GLOBALS.contains(&object) && property == "fetch")
        {
            return Ok(Some(Form::Fetch));
        }
        let property = match function.kind() {
            "member_expression" => function
                .child_by_field_name("property")
                .map(|property| self.text(property)),
            "identifier" if self.axios.here => None,
            _ => return Ok(None),
        };
        if property.is_some_and(|p| !AXIOS_METHODS.iter().any(|(name, _)| *name == p)) {
            return Ok(None);
        }
        let config_at = |name: &str| AXIOS_METHODS.iter().find(|(n, _)| *n == name).map(|m| m.1);
        let method_of = |name: &str| match name {
            "request" => None,
            name => verb(name.strip_suffix("Form").unwrap_or(name)),
        };
        Ok(match self.world.eval(self.at, function, budget)? {
            Value::Package(package, path) if package == "axios" => match &path[..] {
                [] => Some(Form::Axios {
                    base: Base::None,
                    url: None,
                    config: 0,
                    method: None,
                }),
                [name] => config_at(name).map(|config| Form::Axios {
                    base: Base::None,
                    url: (config > 0).then_some(0),
                    config,
                    method: method_of(name),
                }),
                _ => None,
            },
            Value::Made {
                factory: super::js::Factory::Axios(base),
                ..
            } => Some(Form::Axios {
                base,
                url: None,
                config: 0,
                method: None,
            }),
            Value::AxiosMethod(base, name) => config_at(&name).map(|config| Form::Axios {
                base,
                url: (config > 0).then_some(0),
                config,
                method: method_of(&name),
            }),
            _ => None,
        })
    }

    /// The call's path and method.
    fn read(
        &self,
        form: &Form,
        arguments: &[Node<'w>],
        budget: &mut Budget<'_>,
    ) -> Result<(Path, Option<&'static str>)> {
        match form {
            Form::Wrapper(wrapper) => {
                let path = match arguments.first() {
                    Some(path) => self.path(*path, budget)?,
                    None => Path::Unreadable,
                };
                let path = match (path, &wrapper.prefix, wrapper.built_in) {
                    (Path::Read(path, _), None, true) => {
                        Path::Read(path, ClientPrefix::CheckoutFallback)
                    }
                    (Path::Read(path, _), Some(prefix), _) => {
                        Path::Read(join(prefix, &path), ClientPrefix::Joined)
                    }
                    (path, ..) => path,
                };
                let method = match wrapper.method {
                    MethodAt::Argument(at) => match arguments.get(at) {
                        None => Some("GET"),
                        Some(method) => match self.world.eval(self.at, *method, budget)? {
                            Value::Str(text) => verb(&text),
                            _ => None,
                        },
                    },
                    MethodAt::Options(at) => self.options(arguments.get(at).copied(), budget)?,
                };
                Ok((path, method))
            }
            Form::Fetch => {
                let path = match arguments.first() {
                    Some(path) => self.path(*path, budget)?,
                    None => Path::Unreadable,
                };
                Ok((path, self.options(arguments.get(1).copied(), budget)?))
            }
            Form::Axios {
                base,
                url,
                config,
                method,
            } => self.axios(base, *url, *config, *method, arguments, budget),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn axios(
        &self,
        base: &Base,
        url: Option<usize>,
        config: usize,
        method: Option<&'static str>,
        arguments: &[Node<'w>],
        budget: &mut Budget<'_>,
    ) -> Result<(Path, Option<&'static str>)> {
        // `axios(url, config)` as well as `axios(config)`.
        let (url, config) = match (url, arguments.first()) {
            (None, Some(first)) if config == 0 => match self.world.eval(self.at, *first, budget)? {
                Value::Object(..) => (None, 0),
                _ => (Some(0), 1),
            },
            _ => (url, config),
        };
        let options = arguments.get(config).copied();
        let object = match options {
            None => None,
            Some(options) => match self.world.eval(self.at, options, budget)? {
                Value::Object(doc, object) => Some((doc, object)),
                _ => {
                    // An unreadable config can carry any URL, base and method.
                    return Ok((Path::Unreadable, method));
                }
            },
        };
        let method = match method {
            Some(method) => Some(method),
            None => self.options(options, budget)?,
        };
        let url = match (url, object) {
            (Some(at), _) => match arguments.get(at) {
                Some(url) => Some(self.world.eval(self.at, *url, budget)?),
                None => None,
            },
            (None, Some((doc, object))) => match self.world.property(doc, object, "url") {
                Some(url) => Some(self.world.eval(doc, url, budget)?),
                None if spread(object) => Some(Value::Unknown),
                None => None,
            },
            (None, None) => None,
        };
        let Some(Value::Str(url)) = url else {
            return Ok((Path::Unreadable, method));
        };
        let base = match object {
            Some((doc, object)) => match self.world.property(doc, object, "baseURL") {
                Some(value) => match self.world.eval(doc, value, budget)? {
                    Value::Str(text) => Base::Known(text),
                    _ => Base::Unreadable,
                },
                None => base.clone(),
            },
            None => base.clone(),
        };
        if absolute(&url) {
            return Ok((Path::Foreign, method));
        }
        let path = match base {
            Base::None => rooted(&url, ClientPrefix::Written),
            Base::Unreadable => Path::Unreadable,
            Base::Known(base) if base.starts_with('/') && !absolute(&base) => {
                match rooted(&join(&base, &url), ClientPrefix::Joined) {
                    Path::Read(path, prefix) if !base.trim_matches('/').is_empty() => {
                        Path::Read(path, prefix)
                    }
                    Path::Read(path, _) => Path::Read(path, ClientPrefix::Written),
                    other => other,
                }
            }
            Base::Known(_) => Path::Foreign,
        };
        Ok((path, method))
    }

    fn path(&self, node: Node<'w>, budget: &mut Budget<'_>) -> Result<Path> {
        Ok(match self.world.eval(self.at, node, budget)? {
            Value::Str(text) => rooted(&text, ClientPrefix::Written),
            _ => Path::Unreadable,
        })
    }

    /// The `method` of an options object. An absent object, or one without a method and
    /// without a spread, is `GET`.
    fn options(
        &self,
        options: Option<Node<'w>>,
        budget: &mut Budget<'_>,
    ) -> Result<Option<&'static str>> {
        let Some(options) = options else {
            return Ok(Some("GET"));
        };
        let (doc, options) = match unwrap(options).kind() {
            "object" => (self.at, unwrap(options)),
            "identifier" => match self.world.eval(self.at, options, budget)? {
                Value::Object(doc, object) => (doc, object),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        let mut method = None;
        let mut spread = false;
        for member in super::js::children(options) {
            match member.kind() {
                "pair" => {
                    let Some(key) = member.child_by_field_name("key") else {
                        return Ok(None);
                    };
                    if self.world.key_name(doc, key).as_deref() == Some("method") {
                        let Some(value) = member.child_by_field_name("value") else {
                            return Ok(None);
                        };
                        method = Some(match self.world.eval(doc, value, budget)? {
                            Value::Str(text) => verb(&text),
                            _ => None,
                        });
                    }
                }
                "shorthand_property_identifier" if self.world.text(doc, member) == "method" => {
                    method = Some(None);
                }
                // An accessor or method named `method` is present but not a literal.
                "method_definition" => {
                    let Some(name) = member.child_by_field_name("name") else {
                        return Ok(None);
                    };
                    if self.world.key_name(doc, name).as_deref() == Some("method") {
                        method = Some(None);
                    }
                }
                "spread_element" => spread = true,
                _ => {}
            }
        }
        Ok(match method {
            Some(method) => method,
            None if spread => None,
            None => Some("GET"),
        })
    }
}

/// Whether a URL names a scheme or a host.
fn absolute(url: &str) -> bool {
    url.starts_with("//")
        || url.split_once("://").is_some_and(|(scheme, _)| {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        })
}

/// A rooted path cut at its query or fragment; anything else is not a client call.
fn rooted(text: &str, prefix: ClientPrefix) -> Path {
    if !text.starts_with('/') {
        return Path::Foreign;
    }
    let end = text.find(['?', '#']).unwrap_or(text.len());
    Path::Read(text[..end].to_owned(), prefix)
}

/// A client prefix and a path, with one `/` between them.
fn join(prefix: &str, path: &str) -> String {
    format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        path.trim_start_matches('/')
    )
}

/// A client path as segments. A segment with a substitution is a `Param`. Only the one
/// leading `/` is structural: `//health` keeps its empty first segment.
fn client_segments(template: &str) -> Vec<Seg> {
    let mut parts: Vec<&str> = template
        .strip_prefix('/')
        .unwrap_or(template)
        .split('/')
        .collect();
    if parts.last().is_some_and(|last| last.is_empty()) {
        parts.pop();
    }
    parts
        .into_iter()
        .map(|part| {
            if part.contains("{}") {
                Seg::Param
            } else {
                Seg::Literal(part.to_owned())
            }
        })
        .collect()
}

/// The route each call reached: as written, else through the `/api/` wrapper prefix.
pub(super) fn resolve(
    routes: &[Route],
    calls: &[ClientCall],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Option<Match>>> {
    charge(work, calls.len().saturating_mul(routes.len()))?;
    let mut matched = Vec::with_capacity(calls.len());
    for call in calls {
        check()?;
        matched.push(best(routes, call));
    }
    Ok(matched)
}

fn best(routes: &[Route], call: &ClientCall) -> Option<Match> {
    let method = call.method.as_deref()?;
    let segments = call.segments.as_deref()?;
    if call.prefix == ClientPrefix::Joined {
        let route = most_literal(&candidates(routes, method, segments))?;
        return Some(Match {
            route,
            confidence: Confidence::WrapperInferred,
        });
    }
    let written = candidates(routes, method, segments);
    if !written.is_empty() {
        let route = most_literal(&written)?;
        let template = segments.contains(&Seg::Param) || {
            let route = &routes[route];
            route.from_table
                || route.method == ANY
                || route
                    .segments
                    .iter()
                    .any(|seg| !matches!(seg, Seg::Literal(_)))
        };
        return Some(Match {
            route,
            confidence: if template {
                Confidence::Template
            } else {
                Confidence::Exact
            },
        });
    }
    if call.prefix != ClientPrefix::CheckoutFallback {
        return None;
    }
    let prefixed = prefixed(segments)?;
    let route = most_literal(&candidates(routes, method, &prefixed))?;
    Some(Match {
        route,
        confidence: Confidence::WrapperInferred,
    })
}

/// `/api/<rest>` as `/api/checkouts/{root}/<rest>`.
fn prefixed(segments: &[Seg]) -> Option<Vec<Seg>> {
    match segments {
        [Seg::Literal(api), rest @ ..] if api == "api" && !rest.is_empty() => {
            let mut prefixed = vec![
                Seg::Literal("api".into()),
                Seg::Literal("checkouts".into()),
                Seg::Param,
            ];
            prefixed.extend_from_slice(rest);
            Some(prefixed)
        }
        _ => None,
    }
}

/// The routes the client path reaches, ranked by their count of literal segments and then
/// by naming the method rather than accepting any.
fn candidates(routes: &[Route], method: &str, client: &[Seg]) -> Vec<(usize, (usize, bool))> {
    routes
        .iter()
        .enumerate()
        .filter(|(_, route)| {
            (route.method == method || route.method == ANY) && fits(&route.segments, client)
        })
        .map(|(index, route)| {
            let literals = route
                .segments
                .iter()
                .filter(|seg| matches!(seg, Seg::Literal(_)))
                .count();
            (index, (literals, route.method != ANY))
        })
        .collect()
}

/// The one best-ranked candidate. A tie has none.
fn most_literal(candidates: &[(usize, (usize, bool))]) -> Option<usize> {
    let most = candidates.iter().map(|(_, rank)| *rank).max()?;
    let mut top = candidates.iter().filter(|(_, rank)| *rank == most);
    match (top.next(), top.next()) {
        (Some((route, _)), None) => Some(*route),
        _ => None,
    }
}

/// Whether a client path is a path the server route accepts. A client substitution fits a
/// server `{name}` or `{*name}` only. A rest capture takes one segment or more.
fn fits(route: &[Seg], client: &[Seg]) -> bool {
    for (at, seg) in route.iter().enumerate() {
        match (seg, client.get(at)) {
            (Seg::Rest, rest) => return rest.is_some(),
            (Seg::Param, Some(_)) => {}
            (Seg::Literal(wanted), Some(Seg::Literal(written))) if wanted == written => {}
            _ => return false,
        }
    }
    route.len() == client.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interfaces::{Extraction, extract as extract_all};
    use crate::references::{DocumentInput, Snapshot};
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

    fn run(files: &[(&str, &str)]) -> Extraction {
        extract_all(&snapshot(files), &mut Work::new(usize::MAX), &mut || Ok(())).unwrap()
    }

    const SERVER: &str = r#"
async fn h() {}
fn router() -> Router {
    Router::new()
        .route("/api/checkouts", get(h))
        .route("/api/checkouts/{root}/status", get(h))
        .route("/api/checkouts/{root}/jobs/{id}", get(h).post(h))
        .route("/api/checkouts/{root}/jobs/current", get(h))
        .route("/api/checkouts/{root}/files/{*rest}", get(h))
        .route("/health", get(h))
        .route("/dup/a", get(h))
        .route("/dup/{x}", get(h))
}
"#;

    /// The first client call of each source line, as `METHOD path -> route confidence`.
    fn outcomes(client: &str) -> Vec<String> {
        let found = run(&[("src/http.rs", SERVER), ("web/app.js", client)]);
        found
            .calls
            .iter()
            .zip(&found.matched)
            .map(|(call, matched)| {
                let call_text = format!(
                    "{} {}",
                    call.method.as_deref().unwrap_or("?"),
                    call.path_template.as_deref().unwrap_or("?")
                );
                match matched {
                    Some(m) => format!(
                        "{call_text} -> {} {:?}",
                        found.routes[m.route].path_template, m.confidence
                    ),
                    None => format!("{call_text} -> unresolved"),
                }
            })
            .collect()
    }

    #[test]
    fn each_wrapper_reads_its_path_and_method() {
        let client = r#"
function go(api, owner, request, body) {
  api("/health");
  api("/health", "post");
  request("/health", body);
  request("/health", body, { method: "POST" });
  api.request("/health", { method: "DELETE", body });
  owner.request("/health");
  api("/health", "get", undefined, () => 1);
}
async function later(api) {
  await api("/health");
  const data = await api("/health", "GET");
}
"#;
        assert_eq!(
            outcomes(client),
            [
                "GET /health -> /health Exact",
                "POST /health -> unresolved",
                "GET /health -> /health Exact",
                "POST /health -> unresolved",
                "DELETE /health -> unresolved",
                "GET /health -> /health Exact",
                "GET /health -> /health Exact",
                "GET /health -> /health Exact",
                "GET /health -> /health Exact",
            ]
        );
    }

    #[test]
    fn paths_are_cut_at_the_query_and_substitutions_become_segments() {
        let client = r#"
function go(api, id, q) {
  api(`/api/checkouts/x/jobs/${id}?${q}`);
  api("/health#top");
  api("/health/");
  api(`/health`);
}
"#;
        assert_eq!(
            outcomes(client),
            [
                "GET /api/checkouts/x/jobs/{} -> /api/checkouts/{root}/jobs/{id} Template",
                "GET /health -> /health Exact",
                "GET /health/ -> /health Exact",
                "GET /health -> /health Exact",
            ]
        );
    }

    #[test]
    fn unreadable_paths_and_methods_are_unresolved_and_foreign_paths_are_not_calls() {
        let client = r#"
function go(api, request, path, verb, opts) {
  api(path);
  api("/health\n");
  api("/health", verb);
  api("/health", "frob");
  request("/health", null, { ...opts });
  request("/health", null, { ...opts, method: "GET" });
  request("/health", null, opts);
  api("https://example.test/health");
  api("health");
  api();
}
"#;
        assert_eq!(
            outcomes(client),
            [
                "GET ? -> unresolved",
                "GET ? -> unresolved",
                "? /health -> unresolved",
                "? /health -> unresolved",
                "? /health -> unresolved",
                "GET /health -> /health Exact",
                "? /health -> unresolved",
                "GET ? -> unresolved",
            ]
        );
    }

    #[test]
    fn matching_as_written_comes_before_the_prefix_and_the_prefix_infers() {
        let client = r#"
function go(api) {
  api("/api/checkouts");
  api("/api/status");
  api("/api/checkouts/r/status");
  api("/api/nothing");
}
"#;
        assert_eq!(
            outcomes(client),
            [
                "GET /api/checkouts -> /api/checkouts Exact",
                "GET /api/status -> /api/checkouts/{root}/status WrapperInferred",
                "GET /api/checkouts/r/status -> /api/checkouts/{root}/status Template",
                "GET /api/nothing -> unresolved",
            ]
        );
    }

    #[test]
    fn the_most_literal_route_wins_and_a_tie_is_unresolved() {
        let client = r#"
function go(api, id) {
  api("/api/checkouts/r/jobs/current");
  api("/api/checkouts/r/jobs/other");
  api("/dup/a");
  api(`/dup/${id}`);
  api("/api/checkouts/r/files/a/b");
  api("/api/checkouts/r/files");
}
"#;
        assert_eq!(
            outcomes(client),
            [
                "GET /api/checkouts/r/jobs/current -> /api/checkouts/{root}/jobs/current Template",
                "GET /api/checkouts/r/jobs/other -> /api/checkouts/{root}/jobs/{id} Template",
                "GET /dup/a -> /dup/a Exact",
                "GET /dup/{} -> /dup/{x} Template",
                "GET /api/checkouts/r/files/a/b -> /api/checkouts/{root}/files/{*rest} Template",
                "GET /api/checkouts/r/files -> unresolved",
            ]
        );
        let tie = r#"
fn router() -> Router {
    Router::new().route("/t/{a}/x", get(h)).route("/t/y/{b}", get(h))
}
async fn h() {}
"#;
        let found = run(&[
            ("src/http.rs", tie),
            ("web/app.js", "function go(api) { api(\"/t/y/x\"); }"),
        ]);
        assert_eq!(found.matched, [None]);
    }

    #[test]
    fn a_typescript_client_and_its_caller_are_read() {
        let found = run(&[
            ("src/http.rs", SERVER),
            (
                "web/client.ts",
                "export async function load(api: Api): Promise<void> {\n  await api<string>(\"/health\");\n}\n",
            ),
        ]);
        let [call] = &found.calls[..] else {
            panic!("{}", found.calls.len());
        };
        assert_eq!(
            serde_json::to_value(Confidence::Unresolved).unwrap(),
            "unresolved"
        );
        assert_eq!((call.range.start, call.range.end), (56, 84));
        assert_eq!(call.lines.start, 2);
        assert!(call.call_id.is_some());
        let caller = serde_json::to_value(call.caller.as_ref().unwrap()).unwrap();
        assert_eq!(caller["name"], "load");
        assert_eq!(found.matched[0].unwrap().confidence, Confidence::Exact);
    }

    #[test]
    fn test_role_clients_are_excluded() {
        let found = run(&[
            ("src/http.rs", SERVER),
            ("web/app.test.js", "function t(api) { api(\"/health\"); }"),
            ("tests/e2e.js", "function t(api) { api(\"/health\"); }"),
            ("web/app.js", "function t(api) { api(\"/health\"); }"),
        ]);
        assert_eq!(found.calls.len(), 1);
        assert_eq!(found.calls[0].document.path, "web/app.js");
    }

    #[test]
    fn an_exhausted_budget_is_a_typed_bounded_work_error() {
        let shot = snapshot(&[
            ("src/http.rs", SERVER),
            ("web/app.js", "function t(api) { api(\"/health\"); }"),
        ]);
        let error = extract_all(&shot, &mut Work::new(10), &mut || Ok(()))
            .err()
            .unwrap();
        assert!(error.to_string().starts_with("bounded_work"), "{error:#}");
    }

    #[test]
    fn a_comment_in_an_argument_list_is_not_an_argument() {
        let client = r#"
function go(api, request) {
  api(/* the path */ "/health");
  api(
    // a note
    "/health",
    /* then the method */ "POST"
  );
  request("/health", /* body */ null, { method: "GET" /* verb */ });
}
"#;
        assert_eq!(
            outcomes(client),
            [
                "GET /health -> /health Exact",
                "POST /health -> unresolved",
                "GET /health -> /health Exact",
            ]
        );
    }

    #[test]
    fn every_phase_over_its_budget_is_a_typed_bounded_work_error() {
        let app = "function t(api) { api(\"/health\"); }";
        let shot = snapshot(&[("src/http.rs", SERVER), ("web/app.js", app)]);
        let mut spent = Work::new(usize::MAX);
        let found = extract_all(&shot, &mut spent, &mut || Ok(())).unwrap();
        assert_eq!(found.calls.len(), 1);
        let whole = spent.used;
        let bounded = |error: anyhow::Error| error.to_string() == crate::interfaces::BUDGET;
        extract_all(&shot, &mut Work::new(whole), &mut || Ok(())).unwrap();
        for short in [0, 1, whole / 2, whole - 1] {
            let error = extract_all(&shot, &mut Work::new(short), &mut || Ok(()))
                .err()
                .unwrap();
            assert!(bounded(error), "{short}");
        }
        let pairs = found.calls.len() * found.routes.len();
        assert!(pairs > 1);
        let mut work = Work::new(pairs - 1);
        assert!(bounded(
            resolve(&found.routes, &found.calls, &mut work, &mut || Ok(()))
                .err()
                .unwrap()
        ));
        resolve(
            &found.routes,
            &found.calls,
            &mut Work::new(pairs),
            &mut || Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn trellis_own_client_reaches_the_dispatch_arm_and_its_test_only_route_is_excluded() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let read = |path: &str| std::fs::read_to_string(dir.join(path)).unwrap();
        let (http, app, cli) = (
            read("src/http.rs"),
            read("web/app.js"),
            read("tests/daemon/cli.rs"),
        );
        let found = run(&[
            ("src/http.rs", &http),
            ("web/app.js", &app),
            ("tests/daemon/cli.rs", &cli),
        ]);
        let reached = |path: &str| {
            let call = found
                .calls
                .iter()
                .zip(&found.matched)
                .find(|(call, _)| call.path_template.as_deref() == Some(path))
                .unwrap_or_else(|| panic!("no call of {path}"));
            call.1.map(|m| (&found.routes[m.route], m.confidence))
        };
        let (route, confidence) = reached("/api/status").expect("matched");
        assert_eq!(route.path_template, "/api/checkouts/{root_key}/status");
        assert_eq!(
            route.handler.kind,
            crate::interfaces::HandlerKind::DispatchArm
        );
        assert_eq!(confidence, Confidence::WrapperInferred);
        let (_, confidence) = reached("/api/checkouts").expect("matched");
        assert_eq!(confidence, Confidence::Exact);
        assert!(
            found
                .routes
                .iter()
                .all(|route| route.handler.document.path != "tests/daemon/cli.rs")
        );
        assert!(
            found
                .routes
                .iter()
                .all(|r| r.path_template != "/api/status")
        );
        assert!(
            found
                .calls
                .iter()
                .all(|c| c.document.path != "tests/daemon/cli.rs")
        );
        assert!(found.matched.iter().any(Option::is_none));
    }
}
