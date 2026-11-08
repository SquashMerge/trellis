//! Client calls: JavaScript and TypeScript calls to a request wrapper, and the matching of
//! each to the server route it reaches.
use super::{
    ClientCall, Confidence, LineIndex, Match, Route, Seg, charge, comment, parse, test_file,
    test_span,
};
use crate::native_evidence::{Call, Range};
use crate::references::{DocumentInput, Snapshot, callers::Callers, work::Work};
use anyhow::Result;
use tree_sitter::Node;

const VERBS: [&str; 7] = ["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"];
/// The receivers of `<owner>.request(path, options)`.
const OWNERS: [&str; 2] = ["api", "owner"];

/// The built-in request wrappers. Each reads the path from argument 0.
#[derive(Clone, Copy)]
enum Wrapper {
    /// `api(path, method)`: the method is a string at argument 1.
    Api,
    /// `request(path, body, options)`: the method is in the options object at argument 2.
    Request,
    /// `api.request(path, options)`: the method is in the options object at argument 1.
    Member,
}

/// What a wrapper's path argument reads as.
enum Path {
    Unreadable,
    /// A readable path that is not rooted: not a client call.
    Foreign,
    Read(String),
}

/// The client calls of every production JavaScript and TypeScript document.
pub(super) fn extract(
    snapshot: &Snapshot,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<ClientCall>> {
    let mut found = Vec::new();
    for doc in &snapshot.documents {
        check()?;
        let language = match doc.key.language.as_str() {
            "javascript" => tree_sitter_javascript::LANGUAGE.into(),
            "typescript" => crate::indexer_ts::grammar(&doc.key.path),
            _ => continue,
        };
        if snapshot.evidence_gaps.contains_key(&doc.key)
            || !doc.calls.iter().any(|call| wrapper_name(doc, call))
            || test_file(doc)
        {
            continue;
        }
        charge(work, doc.file.text.len())?;
        let tree = parse(doc, &language, check)?;
        Document {
            doc,
            source: &doc.file.text,
            root: tree.root_node(),
        }
        .collect(&mut found, check)?;
    }
    Ok(found)
}

/// Whether the call's callee text ends in `api` or `request`.
fn wrapper_name(doc: &DocumentInput, call: &Call) -> bool {
    let text = call
        .callee_range
        .as_ref()
        .and_then(|range| doc.file.text.get(range.start..range.end))
        .or(call.spelling.as_deref())
        .unwrap_or("");
    text.ends_with("api") || text.ends_with("request")
}

struct Document<'a> {
    doc: &'a DocumentInput,
    source: &'a str,
    root: Node<'a>,
}

impl<'a> Document<'a> {
    fn text(&self, node: Node<'_>) -> &'a str {
        self.source.get(node.byte_range()).unwrap_or("")
    }

    fn collect(
        &self,
        found: &mut Vec<ClientCall>,
        check: &mut impl FnMut() -> Result<()>,
    ) -> Result<()> {
        let lines = LineIndex::new(self.source);
        let callers = Callers::new(self.doc);
        let mut sites: Vec<(&Call, Node<'a>, Wrapper)> = Vec::new();
        for call in &self.doc.calls {
            check()?;
            if !wrapper_name(self.doc, call) || test_span(self.doc, &call.range) {
                continue;
            }
            let Some(node) = self.call_node(&call.range).filter(|node| !node.has_error()) else {
                continue;
            };
            let wrapper = node
                .child_by_field_name("function")
                .and_then(|function| self.wrapper(function));
            if let Some(wrapper) = wrapper {
                sites.push((call, node, wrapper));
            }
        }
        sites.sort_by_key(|(call, ..)| (call.range.start, call.range.end));
        for (call, node, wrapper) in sites {
            let arguments = Self::arguments(node);
            let path = match arguments.first() {
                Some(argument) => self.path(*argument),
                None => Path::Unreadable,
            };
            let (template, segments) = match path {
                Path::Foreign => continue,
                Path::Unreadable => (None, None),
                Path::Read(template) => {
                    let segments = client_segments(&template);
                    (Some(template), Some(segments))
                }
            };
            found.push(ClientCall {
                method: self.method(wrapper, &arguments).map(str::to_owned),
                path_template: template,
                segments,
                document: self.doc.key.clone(),
                lines: lines.lines(&call.range),
                range: call.range.clone(),
                call_id: Some(call.id.clone()),
                caller: callers.get(Some(&call.id), &call.range).0,
            });
        }
        Ok(())
    }

    /// The call expression whose span is exactly `range`. A native call over an `await` spans
    /// the awaited expression, so that one reaches the call it awaits.
    fn call_node(&self, range: &Range) -> Option<Node<'a>> {
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

    fn wrapper(&self, function: Node<'a>) -> Option<Wrapper> {
        // The TypeScript grammar reads `await api<T>(path)` as a call of `await api`.
        let function = match function.kind() {
            "await_expression" => function.named_child(0)?,
            _ => function,
        };
        match function.kind() {
            "identifier" => match self.text(function) {
                "api" => Some(Wrapper::Api),
                "request" => Some(Wrapper::Request),
                _ => None,
            },
            "member_expression" => {
                let object = function.child_by_field_name("object")?;
                let property = function.child_by_field_name("property")?;
                (object.kind() == "identifier"
                    && OWNERS.contains(&self.text(object))
                    && self.text(property) == "request")
                    .then_some(Wrapper::Member)
            }
            _ => None,
        }
    }

    fn arguments(call: Node<'a>) -> Vec<Node<'a>> {
        let Some(arguments) = call.child_by_field_name("arguments") else {
            return Vec::new();
        };
        let mut cursor = arguments.walk();
        arguments
            .named_children(&mut cursor)
            .filter(|argument| !comment(*argument))
            .collect()
    }

    /// The text of a string or template literal, with `{}` for each substitution. An escape
    /// sequence or anything unexpected makes it unreadable.
    fn literal(&self, node: Node<'a>) -> Option<String> {
        if !matches!(node.kind(), "string" | "template_string") {
            return None;
        }
        let mut text = String::new();
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            match child.kind() {
                "string_fragment" => text.push_str(self.text(child)),
                "template_substitution" if node.kind() == "template_string" => text.push_str("{}"),
                _ if comment(child) => {}
                _ => return None,
            }
        }
        Some(text)
    }

    fn path(&self, node: Node<'a>) -> Path {
        match self.literal(node) {
            None => Path::Unreadable,
            Some(text) if !text.starts_with('/') => Path::Foreign,
            Some(text) => {
                let end = text.find(['?', '#']).unwrap_or(text.len());
                Path::Read(text[..end].to_owned())
            }
        }
    }

    /// The written method, or `None` when it cannot be read.
    fn method(&self, wrapper: Wrapper, arguments: &[Node<'a>]) -> Option<&'static str> {
        match wrapper {
            Wrapper::Api => match arguments.get(1) {
                None => Some("GET"),
                Some(method) => verb(&self.literal(*method)?),
            },
            Wrapper::Request => self.options(arguments.get(2).copied()),
            Wrapper::Member => self.options(arguments.get(1).copied()),
        }
    }

    /// The `method` of an options object literal. An absent object, or one without a method
    /// and without a spread, is `GET`.
    fn options(&self, options: Option<Node<'a>>) -> Option<&'static str> {
        let Some(options) = options else {
            return Some("GET");
        };
        if options.kind() != "object" {
            return None;
        }
        let mut method = None;
        let mut spread = false;
        let mut cursor = options.walk();
        for member in options.named_children(&mut cursor) {
            match member.kind() {
                "pair" => {
                    let key = member.child_by_field_name("key")?;
                    let name = match key.kind() {
                        "property_identifier" => self.text(key).to_owned(),
                        _ => self.literal(key).unwrap_or_default(),
                    };
                    if name == "method" {
                        let value = member.child_by_field_name("value")?;
                        method = Some(self.literal(value).and_then(|text| verb(&text)));
                    }
                }
                "shorthand_property_identifier" if self.text(member) == "method" => {
                    method = Some(None);
                }
                "spread_element" => spread = true,
                _ => {}
            }
        }
        match method {
            Some(method) => method,
            None if spread => None,
            None => Some("GET"),
        }
    }
}

fn verb(text: &str) -> Option<&'static str> {
    let upper = text.to_ascii_uppercase();
    VERBS.iter().copied().find(|verb| *verb == upper)
}

/// A client path as segments. A segment with a substitution is a `Param`.
fn client_segments(template: &str) -> Vec<Seg> {
    let mut parts: Vec<&str> = template.trim_start_matches('/').split('/').collect();
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
    let written = candidates(routes, method, segments);
    if !written.is_empty() {
        let route = most_literal(&written)?;
        let template = segments.contains(&Seg::Param) || {
            let route = &routes[route];
            route.from_table
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

/// The routes the client path reaches, with each one's count of literal segments.
fn candidates(routes: &[Route], method: &str, client: &[Seg]) -> Vec<(usize, usize)> {
    routes
        .iter()
        .enumerate()
        .filter(|(_, route)| route.method == method && fits(&route.segments, client))
        .map(|(index, route)| {
            let literals = route
                .segments
                .iter()
                .filter(|seg| matches!(seg, Seg::Literal(_)))
                .count();
            (index, literals)
        })
        .collect()
}

/// The one candidate with the most literal segments. A tie has none.
fn most_literal(candidates: &[(usize, usize)]) -> Option<usize> {
    let most = candidates.iter().map(|(_, literals)| *literals).max()?;
    let mut top = candidates.iter().filter(|(_, literals)| *literals == most);
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
