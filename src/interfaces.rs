//! HTTP interface evidence (#788, #855): the server routes a checkout registers and the client
//! calls it makes. Extraction reads parsed source, the stored declaration roles and the
//! captured `package.json` manifests, knows nothing about apps, and runs at read time; nothing
//! here is stored.
mod calls;
mod js;
mod js_routes;
#[cfg(test)]
mod js_tests;
mod routes;

use crate::native_evidence::{Declaration, DeclarationRole, DocumentKey, Range};
use crate::references::{DocumentInput, Snapshot, callers::Caller, work::Work};
use crate::rules::AppScope;
use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use tree_sitter::Node;

/// How firmly a client call is tied to a server route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Confidence {
    Exact,
    Template,
    WrapperInferred,
    Unresolved,
}

/// What the handler anchor points at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum HandlerKind {
    /// A named function declaration.
    Declaration,
    /// An inline closure, anchored to the named callable around it.
    Closure,
    /// A match arm of a dispatch table's answering `match`, anchored to the callable around it.
    DispatchArm,
}

/// 1-based, inclusive line span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Lines {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Handler {
    pub(crate) kind: HandlerKind,
    /// The declaration, or the innermost named callable around the span.
    pub(crate) syntax_id: Option<String>,
    pub(crate) stable_lookup_key: Option<String>,
    pub(crate) document: DocumentKey,
    pub(crate) range: Range,
    pub(crate) lines: Lines,
    /// The dispatch table variant that answers the route.
    pub(crate) variant: Option<String>,
}

/// One path segment. A substitution is `{name}`, a rest capture is `{*name}`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Seg {
    Literal(String),
    Param,
    Rest,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    pub(crate) method: String,
    pub(crate) path_template: String,
    pub(crate) segments: Vec<Seg>,
    /// Expanded from a dispatch table, not registered literally.
    pub(crate) from_table: bool,
    pub(crate) handler: Handler,
}

/// A server route method that answers every method: express `all`, fastify `all`, Koa `all`
/// and a Next.js pages-router API route.
pub(crate) const ANY: &str = "ANY";

/// How a client call's path was assembled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClientPrefix {
    /// As written.
    Written,
    /// As written, else through the built-in checkout prefix rule (#788).
    CheckoutFallback,
    /// With a wrapper's or client's configured prefix or base URL (#855).
    Joined,
}

/// One call a client makes to a server route: a request wrapper, `fetch` or axios.
#[derive(Clone)]
pub(crate) struct ClientCall {
    /// `None` when the method cannot be read.
    pub(crate) method: Option<String>,
    /// `None` when the path cannot be read. A client substitution is written `{}`.
    pub(crate) path_template: Option<String>,
    pub(crate) segments: Option<Vec<Seg>>,
    pub(crate) prefix: ClientPrefix,
    pub(crate) document: DocumentKey,
    pub(crate) range: Range,
    pub(crate) lines: Lines,
    pub(crate) call_id: Option<String>,
    pub(crate) caller: Option<Caller>,
}

/// The route a client call reached, and how firmly. Never `Unresolved`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Match {
    /// Index into [`Extraction::routes`].
    pub(crate) route: usize,
    pub(crate) confidence: Confidence,
}

/// Every route and client call of a revision, and the route each call reached.
#[derive(Clone, Default)]
pub(crate) struct Extraction {
    pub(crate) routes: Vec<Route>,
    pub(crate) calls: Vec<ClientCall>,
    /// One entry per call, in call order. `None` is an unresolved call.
    pub(crate) matched: Vec<Option<Match>>,
    /// The client callees read: the built-in wrappers, `fetch`, axios and configured wrappers.
    pub(crate) callees: Vec<String>,
}

/// The routes and client calls of a snapshot, with each call matched to at most one route.
/// `wrappers` are the rules file's `[[interfaces.wrappers]]` (#855).
pub(crate) fn extract(
    snapshot: &Snapshot,
    wrappers: &[crate::rules::InterfaceWrapper],
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Extraction> {
    let mut routes = routes::extract(snapshot, work, check)?;
    let next_roots = next_roots(snapshot, work, check)?;
    let configured = scoped(wrappers);
    let world = js::World::new(snapshot);
    let mut budget = js::Budget {
        work,
        check: &mut *check,
    };
    let mut found = js_routes::extract(&world, &next_roots, &mut budget)?;
    found.sort_by(|a, b| (&a.path, a.start).cmp(&(&b.path, b.start)));
    let mut seen = BTreeSet::new();
    routes.extend(
        found
            .into_iter()
            .filter(|f| {
                seen.insert((
                    f.route.method.clone(),
                    f.route.path_template.clone(),
                    f.path.clone(),
                ))
            })
            .map(|f| f.route),
    );
    let calls = calls::extract(&world, &configured, &mut budget)?;
    let matched = calls::resolve(&routes, &calls, work, check)?;
    let mut callees: Vec<String> = calls::built_in().into_iter().map(|w| w.callee).collect();
    callees.extend(["fetch".to_owned(), "axios".to_owned()]);
    for wrapper in wrappers {
        if !callees.contains(&wrapper.callee) {
            callees.push(wrapper.callee.clone());
        }
    }
    Ok(Extraction {
        routes,
        calls,
        matched,
        callees,
    })
}

/// The configured wrappers grouped by root, the deepest root first, so a wrapper authored
/// for a directory wins over one for the whole checkout.
fn scoped(wrappers: &[crate::rules::InterfaceWrapper]) -> Vec<calls::Scoped> {
    let mut scopes: Vec<calls::Scoped> = Vec::new();
    for wrapper in wrappers {
        let root = if wrapper.root == "." {
            String::new()
        } else {
            wrapper.root.clone()
        };
        let method = match wrapper.method {
            crate::rules::WrapperMethod::Options(at) => calls::MethodAt::Options(at),
            crate::rules::WrapperMethod::Argument(at) => calls::MethodAt::Argument(at),
        };
        let configured = calls::Wrapper {
            callee: wrapper.callee.clone(),
            method,
            prefix: wrapper.prefix.clone(),
            built_in: false,
        };
        match scopes.iter_mut().find(|scope| scope.root == root) {
            Some(scope) => scope.wrappers.push(configured),
            None => scopes.push(calls::Scoped {
                root,
                wrappers: vec![configured],
            }),
        }
    }
    scopes.sort_by_key(|scope| std::cmp::Reverse(scope.root.len()));
    scopes
}

/// The directories of Next.js apps: each captured `package.json` that depends on `next`. A
/// manifest that is not a JSON object contributes nothing.
fn next_roots(
    snapshot: &Snapshot,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<String>> {
    let mut roots = Vec::new();
    for manifest in &snapshot.manifests {
        check()?;
        let Some(directory) = manifest
            .path
            .strip_suffix("package.json")
            .filter(|rest| rest.is_empty() || rest.ends_with('/'))
            .map(|rest| rest.trim_end_matches('/').to_owned())
        else {
            continue;
        };
        let Some(bytes) = &manifest.bytes else {
            continue;
        };
        charge(work, bytes.len())?;
        let Ok(Value::Object(fields)) = serde_json::from_slice::<Value>(bytes) else {
            continue;
        };
        if ["dependencies", "devDependencies", "peerDependencies"]
            .iter()
            .any(|key| fields.get(*key).and_then(|deps| deps.get("next")).is_some())
        {
            roots.push(directory);
        }
    }
    Ok(roots)
}

/// The three lists of an interface that detail pages serve.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum List {
    Matched,
    ServerOnly,
    ClientOnly,
}

impl List {
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "matched" => Some(Self::Matched),
            "serverOnly" => Some(Self::ServerOnly),
            "clientOnly" => Some(Self::ClientOnly),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::ServerOnly => "serverOnly",
            Self::ClientOnly => "clientOnly",
        }
    }
}

/// One item of one list: the interface, the list, and the position in that list.
#[derive(Clone, Copy)]
pub(crate) struct Row {
    pair: usize,
    list: List,
    at: usize,
}

/// One ordered pair of apps with at least one matched call.
struct Pair {
    client: String,
    server: String,
    /// Each matched route with the call sites that reached it, in route order.
    matched: Vec<(usize, Vec<usize>)>,
    server_only: Vec<usize>,
    client_only: Vec<usize>,
}

/// The extraction seen as interfaces between apps (#788). Each end belongs to the app and the
/// layer that own its file. Built per read; nothing here is stored.
pub(crate) struct Projection<'a> {
    extraction: &'a Extraction,
    route_layers: Vec<Option<String>>,
    call_layers: Vec<Option<String>>,
    pairs: Vec<Pair>,
}

/// Projects the extraction onto the detected apps. `layers` maps each file to its layer.
pub(crate) fn project<'a>(
    extraction: &'a Extraction,
    scope: &AppScope<'_>,
    layers: &BTreeMap<&str, Option<&str>>,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Projection<'a>> {
    let layer_of = |path: &str| layers.get(path).copied().flatten().map(str::to_owned);
    let app_of = |path: &str, layer: &Option<String>| scope.file(path, layer.as_deref());
    let mut route_layers = Vec::with_capacity(extraction.routes.len());
    let mut route_apps = Vec::with_capacity(extraction.routes.len());
    for route in &extraction.routes {
        check()?;
        let layer = layer_of(&route.handler.document.path);
        route_apps.push(app_of(&route.handler.document.path, &layer));
        route_layers.push(layer);
    }
    let mut call_layers = Vec::with_capacity(extraction.calls.len());
    let mut call_apps = Vec::with_capacity(extraction.calls.len());
    for call in &extraction.calls {
        check()?;
        let layer = layer_of(&call.document.path);
        call_apps.push(app_of(&call.document.path, &layer));
        call_layers.push(layer);
    }
    let mut reached = BTreeSet::new();
    let mut grouped: BTreeMap<(usize, usize), BTreeMap<usize, Vec<usize>>> = BTreeMap::new();
    for (call, found) in extraction.matched.iter().enumerate() {
        check()?;
        let Some(found) = found else { continue };
        reached.insert(found.route);
        if let (Some(client), Some(server)) = (call_apps[call], route_apps[found.route])
            && client != server
        {
            grouped
                .entry((client, server))
                .or_default()
                .entry(found.route)
                .or_default()
                .push(call);
        }
    }
    let mut pairs = Vec::with_capacity(grouped.len());
    for ((client, server), matched) in grouped {
        check()?;
        let unmatched = |apps: &[Option<usize>], app: usize, keep: &dyn Fn(usize) -> bool| {
            (0..apps.len())
                .filter(|&at| apps[at] == Some(app) && keep(at))
                .collect::<Vec<_>>()
        };
        let name = |app: usize| scope.name(app).unwrap_or_default().to_owned();
        pairs.push(Pair {
            client: name(client),
            server: name(server),
            matched: matched.into_iter().collect(),
            server_only: unmatched(&route_apps, server, &|route| !reached.contains(&route)),
            client_only: unmatched(&call_apps, client, &|call| {
                extraction.matched[call].is_none()
            }),
        });
    }
    Ok(Projection {
        extraction,
        route_layers,
        call_layers,
        pairs,
    })
}

/// The position of a confidence in counts, best first.
fn confidence_slot(confidence: Confidence) -> usize {
    match confidence {
        Confidence::Exact => 0,
        Confidence::Template => 1,
        Confidence::WrapperInferred => 2,
        Confidence::Unresolved => 3,
    }
}

impl Projection<'_> {
    fn confidence(&self, call: usize) -> Confidence {
        self.extraction.matched[call].map_or(Confidence::Unresolved, |found| found.confidence)
    }

    fn handler(&self, route: usize) -> Value {
        let handler = &self.extraction.routes[route].handler;
        json!({
            "kind": handler.kind,
            "syntaxId": handler.syntax_id,
            "stableLookupKey": handler.stable_lookup_key,
            "path": handler.document.path,
            "lines": handler.lines,
            "variant": handler.variant,
            "layer": self.route_layers[route],
        })
    }

    fn call_site(&self, call: usize) -> Value {
        let site = &self.extraction.calls[call];
        json!({
            "document": site.document,
            "range": site.range,
            "lines": site.lines,
            "callId": site.call_id,
            "caller": site.caller,
            "confidence": self.confidence(call),
            "layer": self.call_layers[call],
        })
    }

    /// The summary entries, one per interface, in client then server order.
    pub(crate) fn summary(&self) -> Vec<Value> {
        self.pairs
            .iter()
            .map(|pair| self.pair_summary(pair))
            .collect()
    }

    fn pair_summary(&self, pair: &Pair) -> Value {
        type End<'l> = (bool, Option<&'l str>);
        fn end(layer: &Option<String>) -> End<'_> {
            (layer.is_none(), layer.as_deref())
        }
        let mut ends: BTreeMap<(End<'_>, End<'_>), (BTreeSet<usize>, usize)> = BTreeMap::new();
        let mut confidence = [0usize; 4];
        let mut call_sites = 0;
        for (route, calls) in &pair.matched {
            for &call in calls {
                let key = (
                    end(&self.call_layers[call]),
                    end(&self.route_layers[*route]),
                );
                let entry = ends.entry(key).or_default();
                entry.0.insert(*route);
                entry.1 += 1;
                call_sites += 1;
                confidence[confidence_slot(self.confidence(call))] += 1;
            }
        }
        confidence[confidence_slot(Confidence::Unresolved)] += pair.client_only.len();
        let ends: Vec<Value> = ends
            .into_iter()
            .map(|((client, server), (routes, sites))| {
                json!({"clientLayer": client.1, "serverLayer": server.1,
                    "routes": routes.len(), "callSites": sites})
            })
            .collect();
        json!({
            "client": pair.client,
            "server": pair.server,
            "kind": "http",
            "ends": ends,
            "wrappers": {
                "callees": self.extraction.callees,
                "prefix": {"client": "/api/", "server": "/api/checkouts/{root}/"},
            },
            "counts": {
                "matchedRoutes": pair.matched.len(),
                "callSites": call_sites,
                "serverOnly": pair.server_only.len(),
                "clientOnly": pair.client_only.len(),
                "confidence": {
                    "exact": confidence[0],
                    "template": confidence[1],
                    "wrapperInferred": confidence[2],
                    "unresolved": confidence[3],
                },
            },
        })
    }

    /// The items of the lists a page asks for, in interface order and then list order. An
    /// absent filter selects everything.
    pub(crate) fn rows<'s>(
        &'s self,
        client: Option<&'s str>,
        server: Option<&'s str>,
        list: Option<List>,
    ) -> impl Iterator<Item = Row> + 's {
        self.pairs
            .iter()
            .enumerate()
            .filter(move |(_, pair)| {
                client.is_none_or(|client| pair.client == client)
                    && server.is_none_or(|server| pair.server == server)
            })
            .flat_map(move |(index, pair)| {
                [
                    (List::Matched, pair.matched.len()),
                    (List::ServerOnly, pair.server_only.len()),
                    (List::ClientOnly, pair.client_only.len()),
                ]
                .into_iter()
                .filter(move |(wanted, _)| list.is_none_or(|list| list == *wanted))
                .flat_map(move |(list, len)| {
                    (0..len).map(move |at| Row {
                        pair: index,
                        list,
                        at,
                    })
                })
            })
    }

    pub(crate) fn item(&self, row: Row) -> Value {
        let pair = &self.pairs[row.pair];
        let entry = match row.list {
            List::Matched => {
                let (route, calls) = &pair.matched[row.at];
                let best = calls
                    .iter()
                    .map(|&call| self.confidence(call))
                    .min_by_key(|&confidence| confidence_slot(confidence))
                    .unwrap_or(Confidence::Unresolved);
                let found = &self.extraction.routes[*route];
                json!({
                    "method": found.method,
                    "pathTemplate": found.path_template,
                    "confidence": best,
                    "handler": self.handler(*route),
                    "callSites": calls.iter().map(|&call| self.call_site(call)).collect::<Vec<_>>(),
                })
            }
            List::ServerOnly => {
                let route = pair.server_only[row.at];
                let found = &self.extraction.routes[route];
                json!({
                    "method": found.method,
                    "pathTemplate": found.path_template,
                    "handler": self.handler(route),
                })
            }
            List::ClientOnly => {
                let call = pair.client_only[row.at];
                let site = &self.extraction.calls[call];
                json!({
                    "method": site.method,
                    "pathTemplate": site.path_template,
                    "confidence": Confidence::Unresolved,
                    "callSite": self.call_site(call),
                })
            }
        };
        json!({"client": pair.client, "server": pair.server, "list": row.list.name(),
            "entry": entry})
    }
}

/// Splits a `/`-rooted template into segments. Only the one leading `/` is removed, so a
/// repeated slash keeps its empty segment. A trailing empty segment is dropped.
pub(crate) fn segments(template: &str) -> Vec<Seg> {
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
            if part.starts_with("{*") {
                Seg::Rest
            } else if part.contains('{') {
                Seg::Param
            } else {
                Seg::Literal(part.to_owned())
            }
        })
        .collect()
}

pub(crate) const BUDGET: &str = "bounded_work: interface extraction budget";

pub(crate) fn charge(work: &mut Work, amount: usize) -> Result<()> {
    anyhow::ensure!(work.charge(amount), BUDGET);
    Ok(())
}

/// Whether the document's file module is test code (Decision 0011). The file module is the
/// single module declaration with no ancestors.
pub(crate) fn test_file(doc: &DocumentInput) -> bool {
    let mut roots = doc
        .declarations
        .iter()
        .filter(|d| d.kind == "module" && d.ancestors.is_empty());
    match (roots.next(), roots.next()) {
        (Some(root), None) => root.role == DeclarationRole::Test,
        _ => false,
    }
}

/// Whether the innermost declaration containing the span is test code.
pub(crate) fn test_span(doc: &DocumentInput, span: &Range) -> bool {
    let size = |d: &&Declaration| d.range.end - d.range.start;
    let containing = || {
        doc.declarations
            .iter()
            .filter(|d| d.range.start <= span.start && span.end <= d.range.end)
    };
    let Some(smallest) = containing().map(|d| size(&d)).min() else {
        return false;
    };
    containing()
        .filter(|d| size(d) == smallest)
        .any(|d| d.role == DeclarationRole::Test)
}

/// The innermost named callable containing the span.
pub(crate) fn named_callable<'d>(doc: &'d DocumentInput, span: &Range) -> Option<&'d Declaration> {
    doc.declarations
        .iter()
        .filter(|d| {
            crate::metrics::callable(&d.kind)
                && d.name.is_some()
                && d.range.start <= span.start
                && span.end <= d.range.end
        })
        .min_by_key(|d| d.range.end - d.range.start)
}

const TEST_SUPPORT_ATTRIBUTE: &str = "#[cfg(feature=\"test-support\")]";

/// Whether the node is a Rust, JavaScript or TypeScript comment.
pub(crate) fn comment(node: Node<'_>) -> bool {
    matches!(node.kind(), "line_comment" | "block_comment" | "comment")
}

fn test_support_attribute(node: Node<'_>, source: &str) -> bool {
    node.kind() == "attribute_item"
        && source.get(node.byte_range()).is_some_and(|text| {
            text.split_whitespace().collect::<String>() == TEST_SUPPORT_ATTRIBUTE
        })
}

/// Whether the node, or an ancestor, carries `#[cfg(feature = "test-support")]` exactly,
/// whitespace aside. Attributes are the node's preceding siblings, or its leading children for
/// a match arm, which owns its own.
pub(crate) fn under_test_support(node: Node<'_>, source: &str) -> bool {
    let mut current = Some(node);
    while let Some(node) = current {
        let mut previous = node.prev_named_sibling();
        while let Some(sibling) = previous {
            if test_support_attribute(sibling, source) {
                return true;
            }
            if !(sibling.kind() == "attribute_item" || comment(sibling)) {
                break;
            }
            previous = sibling.prev_named_sibling();
        }
        if node.kind() == "match_arm" {
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if test_support_attribute(child, source) {
                    return true;
                }
                if !(child.kind() == "attribute_item" || comment(child)) {
                    break;
                }
            }
        }
        current = node.parent();
    }
    false
}

/// Byte offsets to 1-based lines.
pub(crate) struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let starts = std::iter::once(0)
            .chain(text.match_indices('\n').map(|(at, _)| at + 1))
            .collect();
        Self { starts }
    }

    fn line(&self, offset: usize) -> usize {
        self.starts.partition_point(|&start| start <= offset)
    }

    pub(crate) fn lines(&self, range: &Range) -> Lines {
        let last = range.end.saturating_sub(1).max(range.start);
        Lines {
            start: self.line(range.start),
            end: self.line(last),
        }
    }
}

/// Parses one document. An abort from `check` is returned as is.
pub(crate) fn parse(
    doc: &DocumentInput,
    language: &tree_sitter::Language,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<tree_sitter::Tree> {
    check()?;
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(language)?;
    let source = &doc.file.text;
    let mut aborted = None;
    let tree = parser.parse_with_options(
        &mut |offset, _| &source.as_bytes()[offset..],
        None,
        Some(
            tree_sitter::ParseOptions::new().progress_callback(&mut |_| {
                if let Err(e) = check() {
                    aborted = Some(e);
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            }),
        ),
    );
    if let Some(e) = aborted {
        return Err(e);
    }
    tree.context("bounded_work: interface extraction parser stopped")
}
