//! Client calls: JavaScript and TypeScript request wrappers, `fetch` and axios (#788, #855),
//! and the matching of each call to the server route it reaches. Rust `reqwest` calls (#858)
//! are read in `rust_calls` and matched here.
use super::js::{Base, Budget, Prop, Value, World, arguments, unwrap, verb};
use super::{
    ANY, ClientCall, ClientPrefix, Confidence, LineIndex, Match, Route, Seg, charge, test_file,
    test_span,
};
use crate::native_evidence::{Call, Range};
use crate::references::{DocumentInput, callers::Callers, work::Work};
use anyhow::Result;
use std::collections::BTreeMap;
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

/// Which files configured wrappers apply to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Scope {
    /// Files under a checkout-relative directory; empty is every file.
    Root(String),
    /// Files assigned to an app.
    App(String),
}

/// Configured wrappers that apply to one scope.
pub(crate) struct Scoped {
    pub(crate) scope: Scope,
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
    apps: &BTreeMap<String, String>,
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
            .filter(|scoped| match &scoped.scope {
                Scope::Root(root) => under(&doc.key.path, root),
                Scope::App(app) => apps.get(&doc.key.path) == Some(app),
            })
            .flat_map(|scope| scope.wrappers.iter().cloned())
            .collect();
        for wrapper in built_in() {
            if !wrappers.iter().any(|w| w.callee == wrapper.callee) {
                wrappers.push(wrapper);
            }
        }
        let axios = axios_anywhere;
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

/// Whether a native call may be a client call, by the last name of its callee.
/// `axios` is whether any document mentions axios: only then can an identifier or an
/// axios method name be an axios client, whatever the calling file itself says.
fn candidate(doc: &DocumentInput, call: &Call, wrappers: &[Wrapper], axios: bool) -> bool {
    let text = callee_text(doc, call);
    let last = text
        .rsplit(['.', ' ', '\n', '\t', '\r'])
        .next()
        .unwrap_or(text);
    last == "fetch"
        || (axios && !text.contains('.'))
        || (axios && AXIOS_METHODS.iter().any(|(name, _)| *name == last))
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
    axios: bool,
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

    /// The call expression measured at exactly `range`. A native call over an `await` spans
    /// the awaited expression, so that one reaches the call it awaits; the `await f<T>(x)`
    /// misparse is measured from its callee (#958).
    fn call_node(&self, range: &Range) -> Option<Node<'w>> {
        let mut node = self
            .root
            .descendant_for_byte_range(range.start, range.end)?;
        loop {
            if node.kind() == "call_expression"
                && crate::native_evidence::awaited_callee(node).is_some()
                && crate::native_evidence::js_call_range(node) == *range
            {
                return Some(node);
            }
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
            "identifier" if self.axios => None,
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
                Prop::Found(url) => Some(self.world.eval(doc, url, budget)?),
                Prop::Unknown => Some(Value::Unknown),
                Prop::Absent => None,
            },
            (None, None) => None,
        };
        let Some(Value::Str(url)) = url else {
            return Ok((Path::Unreadable, method));
        };
        let base = match object {
            Some((doc, object)) => match self.world.property(doc, object, "baseURL") {
                Prop::Found(value) => match self.world.eval(doc, value, budget)? {
                    Value::Str(text) => Base::Known(text),
                    _ => Base::Unreadable,
                },
                Prop::Unknown => Base::Unreadable,
                Prop::Absent => base.clone(),
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

    /// The `method` of an options object. An absent object, or one where no member can set
    /// a method, is `GET`.
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
        Ok(match self.world.property(doc, options, "method") {
            Prop::Absent => Some("GET"),
            Prop::Unknown => None,
            Prop::Found(value) => match self.world.eval(doc, value, budget)? {
                Value::Str(text) => verb(&text),
                _ => None,
            },
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
pub(super) fn client_segments(template: &str) -> Vec<Seg> {
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
        extract_all(
            &snapshot(files),
            &[],
            &Default::default(),
            &mut Work::new(usize::MAX),
            &mut || Ok(()),
        )
        .unwrap()
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
        // The awaited call `api<string>("/health")`, without its `await`, as for a call
        // without type arguments (#958).
        assert_eq!((call.range.start, call.range.end), (62, 84));
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
        let error = extract_all(
            &shot,
            &[],
            &Default::default(),
            &mut Work::new(10),
            &mut || Ok(()),
        )
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
        let found =
            extract_all(&shot, &[], &Default::default(), &mut spent, &mut || Ok(())).unwrap();
        assert_eq!(found.calls.len(), 1);
        let whole = spent.used;
        let bounded = |error: anyhow::Error| error.to_string() == crate::interfaces::BUDGET;
        extract_all(
            &shot,
            &[],
            &Default::default(),
            &mut Work::new(whole),
            &mut || Ok(()),
        )
        .unwrap();
        for short in [0, 1, whole / 2, whole - 1] {
            let error = extract_all(
                &shot,
                &[],
                &Default::default(),
                &mut Work::new(short),
                &mut || Ok(()),
            )
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

    const ACTIX_SERVER: &str = r#"
use actix_web::{get, web, App};
#[get("/health")]
async fn health() {}
async fn create() {}
async fn show() {}
async fn status() {}
async fn unused() {}
fn app() -> App {
    App::new()
        .service(health)
        .service(
            web::scope("/api")
                .route("/users", web::post().to(create))
                .service(web::resource("/users/{id}").route(web::get().to(show)))
                .route("/checkouts/{root}/status", web::get().to(status))
                .route("/unused", web::delete().to(unused)),
        )
}
"#;

    const REQWEST_CLIENT: &str = r#"
use reqwest::{Client, Method};
const BASE: &str = "http://localhost:8080";
const HEALTH: &str = "http://localhost:8080/health";
struct Api { client: Client }
impl Api { async fn calls(&self, client: &Client, http_client: Client, id: u32, host: &str, url: String) {
    client.get("http://localhost:8080/health").send().await;
    reqwest::get(HEALTH).await;
    reqwest::blocking::get("/health");
    http_client.post(format!("{BASE}/api/users")).send().await;
    client.get(format!("{}/api/users/{}", BASE, id)).send().await;
    client.get(&format!("{base}/api/users/{id}?full=1", base = BASE)).send().await;
    client.request(Method::GET, format!("http://{host}/api/users/{id}")).send().await;
    client.request(reqwest::Method::DELETE, format!("{BASE}/api/unused"));
    Client::new().get("https://example.test/api/missing").send().await;
    self.client.get(format!("{}/api/users", host)).send().await;
    client.get(url).send().await;
    client.request(verb, "/health");
    client.get("/api/status");
} }
"#;

    #[test]
    fn reqwest_calls_match_actix_routes() {
        let found = run(&[
            ("server/src/main.rs", ACTIX_SERVER),
            ("client/src/lib.rs", REQWEST_CLIENT),
        ]);
        let outcomes: Vec<String> = found
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
            .collect();
        assert_eq!(
            outcomes,
            [
                "GET /health -> /health Exact",
                "GET /health -> /health Exact",
                "GET /health -> /health Exact",
                "POST /api/users -> /api/users Exact",
                "GET /api/users/{} -> /api/users/{id} Template",
                "GET /api/users/{} -> /api/users/{id} Template",
                "GET /api/users/{} -> /api/users/{id} Template",
                "DELETE /api/unused -> /api/unused Exact",
                "GET /api/missing -> unresolved",
                "GET ? -> unresolved",
                "GET ? -> unresolved",
                "? /health -> unresolved",
                "GET /api/status -> unresolved",
            ]
        );
        assert!(
            found
                .calls
                .iter()
                .all(|call| call.prefix == ClientPrefix::Written)
        );
        let first = &found.calls[0];
        assert_eq!(first.lines.start, 7);
        assert_eq!(found.calls[9].lines.start, 16);
        assert!(first.call_id.is_some());
        let caller = serde_json::to_value(first.caller.as_ref().unwrap()).unwrap();
        assert_eq!(caller["name"], "calls");
    }

    #[test]
    fn rust_code_that_is_not_a_reqwest_call_is_not_a_client_call() {
        let lookalikes = r#"
use reqwest::Client;
fn f(map: HashMap<String, String>, cache: Cache, client: Client) {
    map.get("/health");
    cache.post("/health");
    client.send("/health");
    client.get("/health", extra);
    other::get("/health");
}
#[cfg(feature = "test-support")]
fn support(client: Client) { client.get("/health"); }
#[cfg(test)]
mod tests {
    fn t(client: Client) { client.get("/health"); }
}
"#;
        let no_reqwest = "fn g(client: Http) { client.get(\"/health\"); }";
        let in_tests = "use reqwest::Client;\nfn t(client: Client) { client.get(\"/health\"); }";
        let found = run(&[
            ("server/src/main.rs", ACTIX_SERVER),
            ("client/src/lib.rs", lookalikes),
            ("client/src/plain.rs", no_reqwest),
            ("client/tests/api.rs", in_tests),
        ]);
        assert!(found.calls.is_empty(), "{}", found.calls.len());
        assert!(!found.routes.is_empty());
    }

    /// #910: a `.get` counts only on a receiver that is a `reqwest` client by its type or
    /// constructor. A slice, `Vec` or map `.get`, or a name merely containing `client`, is
    /// not a client call, even in a document that mentions `reqwest`.
    #[test]
    fn a_get_on_a_receiver_that_is_not_a_reqwest_client_is_not_a_client_call() {
        let lookalikes = r#"
use reqwest::Client;
struct Cache { client: HashMap<String, String> }
struct Mixed { http: Client }
struct Other { http: Vec<u8> }
fn fits(route: &[Seg], client: &[Seg]) { client.get("/health"); }
fn listed(client: Vec<&str>, clients: HashMap<&str, u8>) {
    client.get("/health");
    clients.get("/health");
}
fn local(cache: Cache, other: Other) {
    let client = vec!["/health"];
    client.get("/health");
    let http_client = Config::new();
    http_client.post("/api/users");
    cache.client.get("/health");
    other.http.get("/health");
    let client = Other::new().get("/health");
}
fn shadowed(client: Client) {
    let client = [1, 2];
    client.get("/health");
}
fn untyped() { client.get("/health"); }
"#;
        assert_eq!(reqwest_paths(lookalikes), Vec::<String>::new());
    }

    /// #921 review: a name bound more than once in its function is never a client, so an
    /// expired inner client does not type an outer map. The other calls here are accepted
    /// losses (precision over recall): `m` and `c` are each bound twice.
    #[test]
    fn a_binding_counts_only_where_it_is_in_scope() {
        let expired = r#"
fn f() {
    let cache = std::collections::HashMap::<&str, u8>::new();
    { let cache = reqwest::Client::new(); }
    cache.get("/health");
}
"#;
        assert_eq!(reqwest_paths(expired), Vec::<String>::new());
        let inner = r#"
use reqwest::Client;
fn f(c: Client, m: HashMap<&str, u8>) {
    { let m = c.clone(); m.get("/inner"); }
    m.get("/outer");
    for c in [1] { c.get("/loop"); }
    c.get("/after");
}
"#;
        assert_eq!(reqwest_paths(inner), Vec::<String>::new());
    }

    /// #921 review: `let c = c.clone()` binds `c` twice, so the call is an accepted loss
    /// (precision over recall), never a false call.
    #[test]
    fn a_let_initializer_reads_the_previous_binding() {
        let same_name = r#"use reqwest::Client; fn f(c: Client) { let c = c.clone(); c.get("/same-name-clone"); }"#;
        assert_eq!(reqwest_paths(same_name), Vec::<String>::new());
    }

    /// #921 review: only a top-level reqwest import that nothing else in the document
    /// declares types a name. A module's import never does, and a root import is unread
    /// once a module declares its own `Client` (an accepted loss).
    #[test]
    fn a_reqwest_import_types_a_name_only_where_it_is_in_scope() {
        let nested = r#"
mod net { use reqwest::Client; }
struct Client;
impl Client { fn get(&self, _: &str) {} }
fn f(c: Client) { c.get("/health"); }
"#;
        assert_eq!(reqwest_paths(nested), Vec::<String>::new());
        let root = r#"
use reqwest::Client;
fn f(c: Client) { c.get("/root"); }
mod other { struct Client; fn g(c: Client) { c.get("/other"); } }
mod net { use reqwest::Client; fn h(c: &Client) { c.get("/net"); } }
"#;
        assert_eq!(reqwest_paths(root), Vec::<String>::new());
        let only = "use reqwest::Client;\nfn f(c: Client) { c.get(\"/only\"); }\n";
        assert_eq!(reqwest_paths(only), ["GET /only"]);
    }

    /// #921 re-review: any other import, item or type parameter named `Client` in the
    /// document makes the name ambiguous, so it is never a client; `/kept` and `/explicit`
    /// are accepted losses (precision over recall).
    #[test]
    fn a_nearer_declaration_of_the_name_hides_the_reqwest_import() {
        let nearer = r#"
use reqwest::Client;
fn f() {
    {
        use std::collections::HashMap as Client;
        let mut cache = Client::new();
        cache.insert("/fake", 1u8);
        cache.get("/fake");
    }
}
"#;
        assert_eq!(reqwest_paths(nearer), Vec::<String>::new());
        let others = r#"
use reqwest::Client;
fn glob(c: Client) { use std::collections::*; let d: Client = make(); d.get("/glob"); }
fn generic<Client: Get>(c: Client) { c.get("/generic"); }
fn item() { struct Client; fn g(c: Client) { c.get("/item"); } }
fn kept(c: Client) { c.get("/kept"); }
mod inner { use reqwest::*; use reqwest::Client; fn h(c: Client) { c.get("/explicit"); } }
"#;
        assert_eq!(reqwest_paths(others), Vec::<String>::new());
    }

    /// #921 re-review: every name a pattern binds, a shorthand field among them, makes the
    /// name ambiguous in its function; `/kept` is an accepted loss.
    #[test]
    fn every_name_a_pattern_binds_hides_an_outer_client() {
        let shorthand = r#"
struct Slot { c: std::collections::HashMap<&'static str, u8> }
fn f(c: reqwest::Client, slot: Slot) {
    let Slot { c } = slot;
    c.get("/fake");
}
"#;
        assert_eq!(reqwest_paths(shorthand), Vec::<String>::new());
        let others = r#"
fn f(c: reqwest::Client, pair: (u8, Map), slot: Slot) {
    { let (_, c) = pair; c.get("/tuple"); }
    { let Slot { ref c, .. } = slot; c.get("/ref"); }
    { let c @ Slot { .. } = slot; c.get("/at"); }
    { let [c, ..] = maps; c.get("/slice"); }
    if let Some(Slot { c }) = found { c.get("/if-let"); }
    c.get("/kept");
}
"#;
        assert_eq!(reqwest_paths(others), Vec::<String>::new());
    }

    /// #921 fourth review: a match guard's pattern binding and a nearer module item are
    /// never a client. A const generic named `Client` makes the import ambiguous, so the
    /// real call is not read: a lost call is accepted, a false one is not.
    #[test]
    fn a_guard_binding_a_module_item_and_a_const_generic_make_the_name_ambiguous() {
        let guard = r#"
struct Slot { c: std::collections::HashMap<&'static str, u8> }
fn f(c: reqwest::Client, slot: Slot) {
    match slot {
        Slot { c } if c.get("/fake").is_some() => (),
        _ => ()
    }
}
"#;
        assert_eq!(reqwest_paths(guard), Vec::<String>::new());
        let module = r#"
use reqwest::Client;
fn f() {
    mod Client {
        pub fn new() -> std::collections::HashMap<&'static str, u8> {
            std::collections::HashMap::new()
        }
    }
    let mut cache = Client::new();
    cache.insert("/fake", 1u8);
    cache.get("/fake");
}
"#;
        assert_eq!(reqwest_paths(module), Vec::<String>::new());
        let generic = r#"
use reqwest::Client;
fn kept<const Client: usize>(c: Client) {
    c.get("/kept");
}
"#;
        // Accepted loss (precision over recall).
        assert_eq!(reqwest_paths(generic), Vec::<String>::new());
    }

    /// #921 fifth review: a glob below the top level may bring in another type of the
    /// imported name, so the imported name is never read while one exists.
    #[test]
    fn a_local_glob_makes_the_imported_client_name_ambiguous() {
        let control =
            "use reqwest::Client as Value;\nfn control(c: Value) { c.get(\"/control-glob\"); }\n";
        assert_eq!(reqwest_paths(control), ["GET /control-glob"]);
        let fake = r#"
use reqwest::Client as Value;
fn f() {
    use serde_json::*;
    let c: Value = serde_json::json!({"/fake": 1});
    c.get("/fake");
}
"#;
        assert_eq!(reqwest_paths(fake), Vec::<String>::new());
        let client = r#"
mod cache;
use reqwest::Client;

pub fn real(c: Client) { c.get("http://localhost/real"); }
pub fn cached() {
    use crate::cache::*;
    let c: Client = Default::default();
    c.get("/fake");
}
"#;
        let cache = "pub type Client = std::collections::HashMap<&'static str, u8>;\n";
        // `/real` is an accepted loss (precision over recall).
        let found = run(&[
            ("client/src/lib.rs", client),
            ("client/src/cache.rs", cache),
        ]);
        assert!(found.calls.is_empty(), "{}", found.calls.len());
    }

    /// #921 fifth review: a named item in the function, such as a block's `const c`,
    /// binds the name too, so the typed parameter `c` is ambiguous.
    #[test]
    fn a_local_item_of_the_name_makes_a_typed_parameter_ambiguous() {
        let control = "fn control(c: reqwest::Client) { c.get(\"/control-const\"); }\n";
        assert_eq!(reqwest_paths(control), ["GET /control-const"]);
        let fake = r#"
#![allow(non_upper_case_globals, unused_variables)]
struct Map;
impl Map { fn get(&self, _: &str) {} }
fn f(c: reqwest::Client) {
    {
        const c: Map = Map;
        c.get("/fake");
    }
}
"#;
        assert_eq!(reqwest_paths(fake), Vec::<String>::new());
        let globbed = "fn f(c: reqwest::Client) { use m::*; c.get(\"/glob\"); }\n";
        assert_eq!(reqwest_paths(globbed), Vec::<String>::new());
    }

    /// #921 fifth review: `self.client` counts only inside an `impl` of the struct that
    /// declares the client field.
    #[test]
    fn a_self_field_counts_only_in_an_impl_of_its_struct() {
        let client = r#"
mod models;
use models::Cache;

struct Api { client: reqwest::Client }
impl Cache {
    pub fn cached(&self) -> Option<&u8> {
        self.client.get("/fake")
    }
}
"#;
        let models = r#"
pub struct Cache {
    pub client: std::collections::HashMap<&'static str, u8>,
}
"#;
        let found = run(&[
            ("client/src/lib.rs", client),
            ("client/src/models.rs", models),
        ]);
        assert!(found.calls.is_empty(), "{}", found.calls.len());
        assert_eq!(reqwest_paths(client), Vec::<String>::new());
        let owner = r#"
struct Source<'a> { client: reqwest::Client, name: &'a str }
impl Source<'_> { fn f(&self) { self.client.get("/generic-impl"); } }
impl Clone for Source<'_> { fn clone(&self) -> Self { self.client.get("/trait-impl"); todo!() } }
"#;
        assert_eq!(
            reqwest_paths(owner),
            ["GET /generic-impl", "GET /trait-impl"]
        );
    }

    /// #921 sixth review: a module or a glob could make the impl's name another type, so a
    /// `self` field counts only with the struct and the impl both at the top level of a
    /// document that has no top-level glob import. A nested owner is an accepted loss.
    #[test]
    fn a_self_field_counts_only_at_top_level_without_globs() {
        let models = r#"
pub struct Api {
    pub client: std::collections::HashMap<&'static str, u8>,
}
"#;
        let paths = |client: &str| -> Vec<String> {
            run(&[
                ("client/src/lib.rs", client),
                ("client/src/models.rs", models),
            ])
            .calls
            .iter()
            .map(|call| {
                format!(
                    "{} {}",
                    call.method.as_deref().unwrap_or("?"),
                    call.path_template.as_deref().unwrap_or("?")
                )
            })
            .collect()
        };
        let nested_glob = r#"
mod models;
struct Api { client: reqwest::Client }

mod cached {
    use crate::models::*;
    impl Api {
        pub fn cached(&self) -> Option<&u8> {
            self.client.get("/fake")
        }
    }
}
"#;
        assert_eq!(paths(nested_glob), Vec::<String>::new());
    }

    /// #921 sixth review, second variant: a top-level glob can make the impl's name another
    /// document's type while a nested struct declares the only client field.
    #[test]
    fn a_top_level_glob_disables_a_self_field() {
        let models = r#"
pub struct Api {
    pub client: std::collections::HashMap<&'static str, u8>,
}
"#;
        let paths = |client: &str| -> Vec<String> {
            run(&[
                ("client/src/lib.rs", client),
                ("client/src/models.rs", models),
            ])
            .calls
            .iter()
            .map(|call| {
                format!(
                    "{} {}",
                    call.method.as_deref().unwrap_or("?"),
                    call.path_template.as_deref().unwrap_or("?")
                )
            })
            .collect()
        };
        let top_glob = r#"
mod owned {
    pub struct Api { pub client: reqwest::Client }
}
mod models;
use models::*;
impl Api {
    fn cached(&self) -> Option<&u8> {
        self.client.get("/fake")
    }
}
"#;
        assert_eq!(paths(top_glob), Vec::<String>::new());
        let nested_owner = r#"
mod owned {
    pub struct Api { pub client: reqwest::Client }
    impl Api {
        pub fn control(&self) { self.client.get("/control"); }
    }
}
mod models;
"#;
        // Accepted loss (precision over recall).
        assert_eq!(paths(nested_owner), Vec::<String>::new());
        let direct = r#"
struct Api { client: reqwest::Client }
impl Api {
    pub fn request(&self) { self.client.get("http://localhost/control-self"); }
}
"#;
        assert_eq!(paths(direct), ["GET /control-self"]);
        // A glob inside a nested module cannot change the top-level names, so it does not
        // block the call.
        let tests_glob = r#"
struct Api { client: reqwest::Client }
impl Api {
    fn request(&self) { self.client.get("/with-test-glob"); }
}
#[cfg(test)]
mod tests {
    use super::*;
}
"#;
        assert_eq!(paths(tests_glob), ["GET /with-test-glob"]);
        let imported = "use other::Api;\nstruct Api { client: reqwest::Client }\nimpl Api { fn f(&self) { self.client.get(\"/imported\"); } }\n";
        assert_eq!(reqwest_paths(imported), Vec::<String>::new());
    }

    /// #921 review: a chain is a client only while every step keeps the client type. A
    /// request's headers are not a client, nor is another type's `new()`.
    #[test]
    fn only_a_chain_that_yields_a_reqwest_client_is_a_client() {
        let headers = r#"
use reqwest::Client;
fn f() {
    let headers = Client::new().get("http://h/real").build().unwrap().headers().clone();
    headers.get("/fake");
}
"#;
        assert_eq!(reqwest_paths(headers), ["GET /real"]);
        let others = r#"
use reqwest::Client;
fn f() {
    HeaderMap::new().get("/header-map");
    Client::builder().get("/builder");
    Client::builder().build().get("/result");
    Client::new().clone().get("/cloned");
    Client::builder().https_only(true).build().expect("client").get("/expected");
}
"#;
        assert_eq!(reqwest_paths(others), ["GET /cloned", "GET /expected"]);
    }

    /// #910 positive controls: the client types and constructors still count.
    #[test]
    fn a_get_on_a_reqwest_client_by_type_or_constructor_is_a_client_call() {
        let clients = r#"
use reqwest::{blocking, Client as Http};
struct Api { client: reqwest::Client, plain: Http }
impl Api {
    async fn calls(&self, c: &Http, b: reqwest::blocking::Client) -> Result<()> {
        reqwest::Client::new().get("http://h/new").send().await?;
        reqwest::Client::builder().timeout(T).build()?.get("http://h/built").send().await?;
        Http::new().clone().delete("/clone");
        Http::builder().build().expect("client").post("/expect");
        let typed: Http = make();
        typed.put("/typed");
        c.get("/param");
        b.head("/blocking");
        self.client.get("/field");
        self.plain.patch("/alias");
        Ok(())
    }
}
"#;
        assert_eq!(
            reqwest_paths(clients),
            [
                "GET /new",
                "GET /built",
                "DELETE /clone",
                "POST /expect",
                "PUT /typed",
                "GET /param",
                "HEAD /blocking",
                "GET /field",
                "PATCH /alias",
            ]
        );
        // Accepted losses (precision over recall): an untyped `let`, a field read through
        // anything but `self`, and a field name two structs declare.
        let lost = r#"
use reqwest::Client;
struct Api { client: Client }
struct Other { client: Client }
fn f(api: &Api) {
    let made = Client::new();
    made.get("/untyped-let");
    api.client.get("/not-self");
}
impl Api { fn g(&self) { self.client.get("/two-structs"); } }
"#;
        assert_eq!(reqwest_paths(lost), Vec::<String>::new());
    }

    #[test]
    fn an_exhausted_budget_while_reading_reqwest_calls_is_typed() {
        let shot = snapshot(&[("client/src/lib.rs", REQWEST_CLIENT)]);
        let mut work = Work::new(REQWEST_CLIENT.len() - 1);
        let error = crate::interfaces::rust_calls::extract(&shot, &mut work, &mut || Ok(()))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), crate::interfaces::BUDGET);
        // The source, then each substituted constant or literal (#867 review).
        let substituted = "http://localhost:8080/health".len() + 4 * "http://localhost:8080".len();
        let mut work = Work::new(REQWEST_CLIENT.len() + substituted);
        assert_eq!(
            crate::interfaces::rust_calls::extract(&shot, &mut work, &mut || Ok(()))
                .unwrap()
                .len(),
            13
        );
        let mut work = Work::new(REQWEST_CLIENT.len() + substituted - 1);
        let error = crate::interfaces::rust_calls::extract(&shot, &mut work, &mut || Ok(()))
            .err()
            .unwrap();
        assert_eq!(error.to_string(), crate::interfaces::BUDGET);
    }

    /// The `(method, path)` each reqwest call of `client` reads as.
    fn reqwest_paths(client: &str) -> Vec<String> {
        run(&[("client/src/lib.rs", client)])
            .calls
            .iter()
            .map(|call| {
                format!(
                    "{} {}",
                    call.method.as_deref().unwrap_or("?"),
                    call.path_template.as_deref().unwrap_or("?")
                )
            })
            .collect()
    }

    /// #867 review P1: a `format!` that repeats a constant expands past the budget although
    /// its source is small. The expansion is charged before it is built.
    #[test]
    fn a_format_expansion_over_the_budget_is_typed_bounded_work() {
        let client = format!(
            "use reqwest::Client;\nconst P: &str = \"{}\";\nfn f(client: Client) {{ client.get(format!(\"http://h/{}\")); }}\n",
            "a".repeat(1000),
            "{P}".repeat(50)
        );
        let shot = snapshot(&[("client/src/lib.rs", &client)]);
        let error = crate::interfaces::rust_calls::extract(
            &shot,
            &mut Work::new(client.len() * 4),
            &mut || Ok(()),
        )
        .err()
        .unwrap();
        assert_eq!(error.to_string(), crate::interfaces::BUDGET);
        let calls =
            crate::interfaces::rust_calls::extract(&shot, &mut Work::new(usize::MAX), &mut || {
                Ok(())
            })
            .unwrap();
        assert_eq!(
            calls[0].path_template.as_ref().unwrap().len(),
            1 + 50 * 1000
        );
    }

    /// #867 review P2: a constant is read only where it is in scope at the call.
    #[test]
    fn a_constant_out_of_scope_at_the_call_is_unresolved() {
        let client = r#"
use reqwest::Client;
const VISIBLE: &str = "http://localhost/visible";
mod unrelated {
    const URL: &str = "http://localhost/health";
    const BASE: &str = "http://localhost";
}
fn run(client: &Client, URL: &str, BASE: &str) {
    client.get(URL);
    client.get(format!("{BASE}/x"));
    client.get(VISIBLE);
}
mod inner {
    fn nested(client: &reqwest::Client) { client.get(super::VISIBLE); client.get(VISIBLE); }
}
"#;
        assert_eq!(
            reqwest_paths(client),
            ["GET ?", "GET ?", "GET /visible", "GET ?", "GET ?"]
        );
    }

    /// #867 review (residual P2): any other binding of the name anywhere in the document, an
    /// import or a pattern, makes the top-level constant unreadable.
    #[test]
    fn a_name_also_bound_by_an_import_or_a_pattern_is_unresolved() {
        let aliased = r#"
const URL: &str = "http://localhost/health";
mod config { pub const OTHER: &str = "http://localhost/actual"; }
fn run(client: &reqwest::Client) {
    use crate::config::OTHER as URL;
    client.get(URL);
}
"#;
        assert_eq!(reqwest_paths(aliased), ["GET ?"]);
        for binding in [
            "use crate::config::URL;",
            "let URL = 1;",
            "let (a, URL) = (1, 2);",
            "for URL in [1] {}",
            "if let Some(URL) = None::<u8> {}",
            "match 1 { URL => {} }",
            "let f = |URL: u8| URL;",
        ] {
            let client = format!(
                "const URL: &str = \"http://localhost/health\";\nconst KEPT: &str = \"http://localhost/kept\";\nfn other() {{ {binding} }}\nfn run(client: &reqwest::Client) {{ client.get(URL); client.get(KEPT); }}\n"
            );
            assert_eq!(reqwest_paths(&client), ["GET ?", "GET /kept"], "{binding}");
        }
    }

    /// #867 review (residual P2): a glob import below the top level may bind any name, so it
    /// hides every top-level constant. A top-level glob is shadowed by the constant itself.
    #[test]
    fn a_glob_import_below_the_top_level_hides_every_constant() {
        let local = r#"
const OS: &str = "http://localhost/health";
const KEPT: &str = "http://localhost/kept";
fn run() {
    use std::env::consts::*;
    // A glob in the function would also make a parameter ambiguous (#921), so the
    // receiver here is a constructor chain.
    reqwest::Client::new().get(OS);
}
fn other(client: &reqwest::Client) { client.get(KEPT); }
"#;
        assert_eq!(reqwest_paths(local), ["GET ?", "GET ?"]);
        let top = r#"
use std::env::consts::*;
const OS: &str = "http://localhost/health";
fn run(client: &reqwest::Client) { client.get(OS); }
"#;
        assert_eq!(reqwest_paths(top), ["GET /health"]);
    }

    /// #867 review P2: a substitution with a format specification is not read as its value.
    #[test]
    fn a_format_specification_is_a_substitution_not_its_value() {
        let client = r#"
use reqwest::Client;
const ID: &str = "7";
const BASE: &str = "http://localhost";
fn run(client: &Client) {
    client.get(format!("{BASE}/n/{ID:0>3}"));
    client.get(format!("{BASE}/n/{:?}", ID));
    client.get(format!("{BASE}/n/{0:.1}", ID));
    client.get(format!("{BASE:>40}/n"));
    client.get(format!("{BASE}/n/{ID}"));
}
"#;
        assert_eq!(
            reqwest_paths(client),
            ["GET /n/{}", "GET /n/{}", "GET /n/{}", "GET ?", "GET /n/7"]
        );
    }

    #[test]
    fn trellis_own_client_reaches_the_status_handler_and_its_test_only_route_is_excluded() {
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
        // The arm delegates to exactly one same-document function (#1046), so the handler
        // is that declaration.
        assert_eq!(
            route.handler.kind,
            crate::interfaces::HandlerKind::Declaration
        );
        assert_eq!(
            route.handler.stable_lookup_key.as_deref(),
            Some("src/http.rs::status_answer")
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
