//! #853: which client calls of a Sequence cross to another app's handler, and how firmly.
use super::{
    ClientCall, ClientPrefix, Confidence, Extraction, Handler, HandlerKind, Lines, Match, Route,
    SequenceHop, segments, sequence_hops,
};
use crate::native_evidence::{DocumentKey, Range};
use std::collections::BTreeSet;

fn key(path: &str) -> DocumentKey {
    DocumentKey {
        source_set_id: "s".into(),
        language: if path.ends_with(".rs") {
            "rust"
        } else {
            "javascript"
        }
        .into(),
        path: path.into(),
    }
}

fn route(method: &str, template: &str, path: &str) -> Route {
    Route {
        method: method.into(),
        path_template: template.into(),
        segments: segments(template),
        from_table: false,
        handler: Handler {
            kind: HandlerKind::Declaration,
            syntax_id: Some(format!("syntax:{template}")),
            stable_lookup_key: None,
            document: key(path),
            range: Range { start: 0, end: 1 },
            lines: Lines { start: 1, end: 1 },
            variant: None,
        },
    }
}

fn call(id: &str, method: Option<&str>, template: Option<&str>, path: &str) -> ClientCall {
    ClientCall {
        method: method.map(str::to_owned),
        path_template: template.map(str::to_owned),
        segments: template.map(segments),
        prefix: ClientPrefix::Written,
        document: key(path),
        range: Range { start: 0, end: 1 },
        lines: Lines { start: 1, end: 1 },
        call_id: Some(id.into()),
        caller: None,
    }
}

/// Two apps, `web` (files under `web/`, app 0) and `daemon` (files under `src/`, app 1), and
/// a tooling file in neither. Each call has one confidence or none.
fn extraction() -> Extraction {
    let routes = vec![
        route("POST", "/api/sequence", "src/http.rs"),
        route("GET", "/api/items/{id}", "src/http.rs"),
        route("GET", "/api/status", "src/http.rs"),
        route("GET", "/web/own", "web/server.js"),
        route("GET", "/tools/x", "tools/x.rs"),
    ];
    let calls = vec![
        call("exact", Some("POST"), Some("/api/sequence"), "web/app.js"),
        call("template", Some("GET"), Some("/api/items/{}"), "web/app.js"),
        call("wrapper", Some("GET"), Some("/status"), "web/app.js"),
        call("unresolved", Some("GET"), None, "web/app.js"),
        call("unmatched", Some("GET"), Some("/api/none"), "web/app.js"),
        call("same-app", Some("GET"), Some("/web/own"), "web/app.js"),
        call("no-app", Some("GET"), Some("/tools/x"), "web/app.js"),
        call(
            "not-in-view",
            Some("POST"),
            Some("/api/sequence"),
            "web/other.js",
        ),
    ];
    let found = |route, confidence| Some(Match { route, confidence });
    Extraction {
        routes,
        calls,
        matched: vec![
            found(0, Confidence::Exact),
            found(1, Confidence::Template),
            found(2, Confidence::WrapperInferred),
            None,
            None,
            found(3, Confidence::Exact),
            found(4, Confidence::Exact),
            found(0, Confidence::Exact),
        ],
        callees: vec![],
    }
}

fn app_of(path: &str) -> Option<usize> {
    if path.starts_with("web/") {
        Some(0)
    } else if path.starts_with("src/") {
        Some(1)
    } else {
        None
    }
}

fn hops_for(ids: &[&str]) -> Vec<(String, String)> {
    let extraction = extraction();
    let wanted: BTreeSet<&str> = ids.iter().copied().collect();
    sequence_hops(&extraction, app_of, &wanted, &mut || Ok(()))
        .unwrap()
        .into_iter()
        .map(|(id, hop)| {
            let text = match hop {
                SequenceHop::Cross {
                    route,
                    confidence,
                    client_app,
                    server_app,
                } => format!(
                    "{} {} {confidence:?} {client_app}->{server_app}",
                    route.method, route.path_template
                ),
                SequenceHop::Unresolved { call, client_app } => format!(
                    "unresolved {:?} {:?} {client_app:?}",
                    call.method, call.path_template
                ),
            };
            (id, text)
        })
        .collect()
}

#[test]
fn every_matched_confidence_crosses_to_the_handler_app() {
    assert_eq!(
        hops_for(&["exact", "template", "wrapper"]),
        [
            ("exact".into(), "POST /api/sequence Exact 0->1".into()),
            (
                "template".into(),
                "GET /api/items/{id} Template 0->1".into()
            ),
            (
                "wrapper".into(),
                "GET /api/status WrapperInferred 0->1".into()
            ),
        ]
    );
}

#[test]
fn an_unresolved_or_unmatched_call_stays_a_boundary_with_no_handler() {
    assert_eq!(
        hops_for(&["unresolved", "unmatched"]),
        [
            (
                "unmatched".into(),
                "unresolved Some(\"GET\") Some(\"/api/none\") Some(0)".into()
            ),
            (
                "unresolved".into(),
                "unresolved Some(\"GET\") None Some(0)".into()
            ),
        ]
    );
}

#[test]
fn a_match_within_one_app_or_into_no_app_is_no_hop() {
    assert!(hops_for(&["same-app", "no-app"]).is_empty());
}

#[test]
fn a_call_outside_the_view_or_an_unknown_id_is_no_hop() {
    // Negative control: the extraction's other calls never leak into a view that lacks them.
    assert!(hops_for(&["seed-local-call", "another"]).is_empty());
    assert_eq!(hops_for(&["exact"]).len(), 1);
}

#[test]
fn the_first_call_read_for_an_id_wins() {
    let mut extraction = extraction();
    let mut twin = extraction.calls[3].clone();
    twin.call_id = Some("exact".into());
    extraction.calls.push(twin);
    extraction.matched.push(None);
    let wanted = BTreeSet::from(["exact"]);
    let hops = sequence_hops(&extraction, app_of, &wanted, &mut || Ok(())).unwrap();
    assert!(matches!(
        hops["exact"],
        SequenceHop::Cross {
            confidence: Confidence::Exact,
            ..
        }
    ));
}

#[test]
fn resolution_stops_on_a_failed_check() {
    let extraction = extraction();
    let wanted = BTreeSet::from(["exact"]);
    let outcome = sequence_hops(&extraction, app_of, &wanted, &mut || {
        anyhow::bail!("deadline_exceeded")
    });
    let Err(error) = outcome else {
        panic!("a failed check must stop resolution");
    };
    assert_eq!(error.to_string(), "deadline_exceeded");
}
