#![cfg(feature = "test-support")]
//! Offline catalog and cached-source sequence API checks.
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use tower::ServiceExt;
use trellis::{
    http,
    indexer::{IndexOptions, index_workspace_bundle},
    model::*,
    store::Store,
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
fn setup() -> (
    tempfile::TempDir,
    Store,
    Graph,
    Router,
    Arc<trellis::store::topology::WriterSession>,
) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("a.js"), "function check(x) { return x > 0; }\nfunction seed(x) { function nested() { side(); } if (x) external(x); return x; }\nclass Box { get value() { audit(); return this.x; } method() { save(); } }\n").unwrap();
    std::fs::write(workspace.join("z.js"), "// no methods\n").unwrap();
    let options = IndexOptions::new(workspace.clone());
    let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    // Offline snapshot test; the live reconciliation/read race is tracked in #111.
    state.retain_serving_session_without_tick_for_tests(session.clone());
    let app = http::router(state);
    (dir, store, graph, app, session)
}
async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn catalog_pages_inline_methods_and_conservative_checks() {
    let (_dir, store, graph, app, _session) = setup();
    let pin = store.status().unwrap().revision;
    let (status, page) = call(
        &app,
        "GET",
        &format!(
            "/api/files?indexGeneration={}&indexRevision={}&limit=1",
            pin.index_generation, pin.index_revision
        ),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(page["revision"], json!(pin));
    assert_eq!(page["items"][0]["path"], "a.js");
    assert_eq!(page["nextOffset"], 1);
    let expected = graph
        .nodes
        .iter()
        .filter(|n| n.path == "a.js" && matches!(n.kind, SymbolKind::Function | SymbolKind::Method))
        .count();
    assert_eq!(page["items"][0]["methodCount"], expected);
    let (_, page) = call(
        &app,
        "GET",
        &format!(
            "/api/files?indexGeneration={}&indexRevision={}&limit=1&offset=1",
            pin.index_generation, pin.index_revision
        ),
        Value::Null,
    )
    .await;
    assert_eq!(page["items"][0]["path"], "z.js");
    assert_eq!(page["nextOffset"], Value::Null);
    let (status, methods) = call(
        &app,
        "GET",
        &format!(
            "/api/methods?indexGeneration={}&indexRevision={}&path=a.js",
            pin.index_generation, pin.index_revision
        ),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200);
    let items = methods["items"].as_array().unwrap();
    assert_eq!(items.len(), expected);
    assert!(
        items
            .iter()
            .all(|m| matches!(m["symbol"]["kind"].as_str(), Some("function" | "method")))
    );
    for name in ["check", "nested", "method", "value"] {
        assert!(
            items
                .iter()
                .any(|m| m["symbol"]["name"] == name && m["consequential"] == true),
            "missing {name}: {items:?}"
        );
    }
    assert_eq!(
        call(&app, "GET", "/api/methods?path=z.js", Value::Null)
            .await
            .1["items"],
        json!([])
    );
}
#[tokio::test]
async fn catalog_filter_searches_the_whole_pinned_catalog() {
    let (_dir, store, _graph, app, _session) = setup();
    let pin = store.status().unwrap().revision;
    let page = |query: &str| {
        format!(
            "/api/files?indexGeneration={}&indexRevision={}&limit=1&{query}",
            pin.index_generation, pin.index_revision
        )
    };
    // The filter is applied before pagination: the only match is the last path.
    let (status, found) = call(&app, "GET", &page("q=Z.JS"), Value::Null).await;
    assert_eq!(status, 200, "{found}");
    assert_eq!(found["revision"], json!(pin));
    assert_eq!(
        found["items"],
        json!([{"path":"z.js","language":"javascript","methodCount":0}])
    );
    assert_eq!(found["nextOffset"], Value::Null);
    let (_, both) = call(&app, "GET", &page("q=.js"), Value::Null).await;
    assert_eq!(both["items"][0]["path"], "a.js");
    assert_eq!(both["nextOffset"], 1, "matches page like the catalog");
    let (_, none) = call(&app, "GET", &page("q=missing"), Value::Null).await;
    assert_eq!(none["items"], json!([]));
    assert_eq!(none["nextOffset"], Value::Null);
    for query in ["q=", &format!("q={}", "a".repeat(257)), "q=a%0Ab"] {
        assert_eq!(
            call(&app, "GET", &page(query), Value::Null).await.0,
            400,
            "{query}"
        );
    }
    assert_eq!(
        call(
            &app,
            "GET",
            &page(&format!("q={}", "a".repeat(256))),
            Value::Null
        )
        .await
        .0,
        200,
        "the bound is inclusive"
    );
}
#[tokio::test]
async fn strict_errors_revisions_and_auth() {
    let (_dir, store, graph, app, session) = setup();
    let pin = store.status().unwrap().revision;
    for path in [
        "/api/files?limit=0",
        "/api/files?limit=201",
        "/api/files?extra=1",
        "/api/files?revision=no",
        "/api/methods?path=../a.js",
        "/api/methods?path=a.js&extra=1",
    ] {
        assert_eq!(call(&app, "GET", path, Value::Null).await.0, 400, "{path}");
    }
    assert_eq!(
        call(&app, "GET", "/api/methods?path=missing.js", Value::Null)
            .await
            .0,
        404
    );
    let seed = &graph.nodes.iter().find(|n| n.name == "seed").unwrap().id;
    for body in [
        json!({"seed":seed}),
        json!({"seed":seed,"expectedRevision":pin,"extra":true}),
        json!({"seed":seed,"expectedRevision":pin,"showAll":"yes"}),
        json!({"seed":"","expectedRevision":pin}),
    ] {
        assert_eq!(call(&app, "POST", "/api/sequence", body).await.0, 400);
    }
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/sequence",
            json!({"seed":"missing","expectedRevision":pin})
        )
        .await
        .0,
        404
    );
    for symbol in graph
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, SymbolKind::Class | SymbolKind::Module))
    {
        assert_eq!(
            call(
                &app,
                "POST",
                "/api/sequence",
                json!({"seed":symbol.id,"expectedRevision":pin})
            )
            .await
            .0,
            400
        );
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (_, native, capture) = index_workspace_bundle(
        &IndexOptions::new(_dir.path().join("workspace")),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            pin,
            &cancel,
        )
        .unwrap();
    for path in [
        &format!(
            "/api/files?indexGeneration={}&indexRevision={}",
            pin.index_generation, pin.index_revision
        ),
        &format!(
            "/api/methods?indexGeneration={}&indexRevision={}&path=a.js",
            pin.index_generation, pin.index_revision
        ),
    ] {
        let (status, page) = call(&app, "GET", path, Value::Null).await;
        assert_eq!(status, 200, "{path}: {page}");
        assert_eq!(page["revision"], json!(pin), "{path}: retained revision");
    }
    let (status, sequence) = call(
        &app,
        "POST",
        "/api/sequence",
        json!({"seed":seed,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(status, 200, "{sequence}");
    assert_eq!(sequence["revision"], json!(pin));
    for path in ["/api/files", "/api/methods?path=a.js", "/api/sequence"] {
        let request = Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:7331")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 401);
    }
}
#[tokio::test]
async fn cached_sequence_and_source_are_independent_of_workspace() {
    let (dir, store, graph, app, _session) = setup();
    let pin = store.status().unwrap().revision;
    let seed = &graph.nodes.iter().find(|n| n.name == "seed").unwrap().id;
    let source_path = format!(
        "/api/source?path=a.js&indexGeneration={}&indexRevision={}",
        pin.index_generation, pin.index_revision
    );
    let source_before = call(&app, "GET", &source_path, Value::Null).await;
    assert_eq!(source_before.0, 200, "{:?}", source_before.1);
    let request = json!({"seed":seed,"expectedRevision":pin});
    let sequence_before = call(&app, "POST", "/api/sequence", request.clone()).await;
    assert_eq!(sequence_before.0, 200, "{:?}", sequence_before.1);
    std::fs::remove_file(dir.path().join("workspace/a.js")).unwrap();
    std::fs::remove_file(dir.path().join("workspace/z.js")).unwrap();
    assert_eq!(
        call(&app, "POST", "/api/sequence", request).await,
        sequence_before
    );
    assert_eq!(
        call(&app, "GET", &source_path, Value::Null).await,
        source_before
    );
    assert_eq!(
        call(
            &app,
            "GET",
            &format!(
                "/api/files?indexGeneration={}&indexRevision={}",
                pin.index_generation, pin.index_revision
            ),
            Value::Null
        )
        .await
        .1["items"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(sequence_before.1["revision"], json!(pin));
    assert_eq!(sequence_before.1["seed"]["id"], *seed);
}

use crate::graph_fixtures;

#[tokio::test]
async fn published_sequence_api_pairs_every_call_and_target_with_exact_pin() {
    use graph_fixtures::{Publisher, Spec, bind, syntax_id};
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut publisher = Publisher::open(&dir.path().join("state"), &workspace);
    let mut spec = Spec::new(&[
        ("a.rs", "fn a() { decoy(); decoy(); }\nfn decoy() {}\n"),
        ("target.rs", "fn actual_target() {}\n"),
    ]);
    spec.bindings = (0..2)
        .map(|nth| graph_fixtures::Bind {
            provenance: "shared".into(),
            ..bind("a", nth, "actual_target")
        })
        .collect();
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let target = syntax_id(publisher.native(), "actual_target");
    let calls: Vec<_> = graph_fixtures::calls_of(publisher.native(), "a")
        .iter()
        .map(|call| call.id.clone())
        .collect();
    let store = publisher.store.clone();
    let state = http::new(
        store.clone(),
        IndexOptions::new(workspace.clone()),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let app = http::router(state);
    let (status, view) = call(
        &app,
        "POST",
        "/api/sequence",
        json!({"seed":seed,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    assert_eq!(view["revision"], json!(pin));
    let steps = view["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2);
    for (step, call_id) in steps.iter().zip(calls) {
        assert_eq!(step["callId"], call_id);
        assert_eq!(step["target"], target);
        assert_eq!(step["callEvidence"]["boundaryReason"], "none");
    }
    let participant = view["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|participant| participant["id"] == target)
        .unwrap();
    assert_eq!(participant["declaration"]["path"], "target.rs");
    assert!(
        steps
            .iter()
            .all(|step| step["callEvidence"].get("externalPackage").is_none())
    );
    std::fs::remove_file(workspace.join("target.rs")).unwrap();
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/sequence",
            json!({"seed":seed,"expectedRevision":pin})
        )
        .await
        .1,
        view
    );
}

// #450: an external binding names its SCIP package for in-context library lookup; it
// still creates no target, lane or declaration.
#[tokio::test]
async fn published_sequence_api_names_the_scip_package_of_external_calls() {
    use graph_fixtures::{Publisher, Spec, bind, syntax_id};
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let mut publisher = Publisher::open(&dir.path().join("state"), &workspace);
    let mut spec = Spec::new(&[
        ("a.rs", "fn a() { decoy(); decoy(); }\nfn decoy() {}\n"),
        ("target.rs", "fn actual_target() {}\n"),
    ]);
    spec.external_symbol = "rust-analyzer cargo local-library 0.1.0 candidate_only().".into();
    let mut bound = bind("a", 0, "actual_target");
    bound.resolution = trellis::semantic_evidence::Resolution::External;
    spec.bindings = vec![bound];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let app = http::router(
        http::new(
            publisher.store.clone(),
            IndexOptions::new(workspace.clone()),
            TOKEN.into(),
            "127.0.0.1:7331".parse().unwrap(),
        )
        .unwrap(),
    );
    let (status, view) = call(
        &app,
        "POST",
        "/api/sequence",
        json!({"seed":seed,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(status, 200, "{view}");
    let steps = view["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 2, "{view}");
    assert_eq!(steps[0]["callEvidence"]["boundaryReason"], "external");
    assert_eq!(
        steps[0]["callEvidence"]["externalPackage"],
        json!({"manager":"cargo","name":"local-library","version":"0.1.0"})
    );
    assert!(steps[0].get("target").is_none());
    // The unbound second call has no binding and so no package.
    assert_eq!(
        steps[1]["callEvidence"]["boundaryReason"],
        "missingEvidence"
    );
    assert!(steps[1]["callEvidence"].get("externalPackage").is_none());
    assert!(
        view["participants"]
            .as_array()
            .unwrap()
            .iter()
            .all(
                |participant| participant.get("declaration").is_none() || participant["id"] == seed
            )
    );
}

// #522: the sequence API builds participants from fresh exact bindings and expands each
// direct callee's own body on its lane. Standard-library calls are incidental: counted,
// hidden unless `showAll`. Without fresh evidence it is the syntax view, with the reason.
#[tokio::test]
async fn published_sequence_api_expands_direct_callees_and_hides_incidental_calls() {
    use graph_fixtures::{Publisher, Spec, bind, syntax_id};
    use trellis::semantic_evidence::{Dispatch, Resolution};
    let files = [
        (
            "a.rs",
            "fn a() { std_call(); helper(); }\nfn std_call() {}\n",
        ),
        (
            "helper.rs",
            "pub fn helper() { inner(); std_other(); }\npub fn inner() {}\npub fn std_other() {}\n",
        ),
    ];
    let external = |caller: &str, nth| {
        let mut binding = bind(caller, nth, "std_call");
        binding.resolution = Resolution::External;
        binding.dispatch = Dispatch::Unknown;
        binding
    };
    for stale in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let mut publisher = Publisher::open(&dir.path().join("state"), &workspace);
        let mut spec = Spec::new(&files);
        spec.external_symbol = "rust-analyzer cargo std 1.98.1 vec/Vec#new().".into();
        if stale {
            spec.requested_version = "v2".into();
        }
        spec.bindings = vec![
            external("a", 0),
            bind("a", 1, "helper"),
            bind("helper", 0, "inner"),
            external("helper", 1),
        ];
        let pin = publisher.publish(&spec);
        let (seed, helper, inner) = (
            syntax_id(publisher.native(), "a"),
            syntax_id(publisher.native(), "helper"),
            syntax_id(publisher.native(), "inner"),
        );
        let app = http::router(
            http::new(
                publisher.store.clone(),
                IndexOptions::new(workspace.clone()),
                TOKEN.into(),
                "127.0.0.1:7331".parse().unwrap(),
            )
            .unwrap(),
        );
        let sequence = |show_all: bool| {
            let app = app.clone();
            let body = json!({"seed":seed,"expectedRevision":pin,"showAll":show_all});
            async move { call(&app, "POST", "/api/sequence", body).await }
        };
        let (status, view) = sequence(false).await;
        assert_eq!(status, 200, "{view}");
        let steps = view["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2, "{view}");
        if stale {
            assert_eq!(view["semantic"]["tier"], "syntax");
            assert_eq!(
                view["semantic"]["reason"],
                "Semantic evidence for a.rs is stale: it was imported for an earlier version of the file. Showing syntax."
            );
            assert!(steps.iter().all(|step| step.get("callEvidence").is_none()
                && step.get("target").is_none()
                && step["children"].as_array().unwrap().is_empty()
                && step["hidden"] == false));
            assert_eq!(view["incidentalSteps"], 0);
            continue;
        }
        assert_eq!(
            view["semantic"],
            json!({"tier":"semantic","producerId":graph_fixtures::PRODUCER})
        );
        let lanes: Vec<_> = view["participants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|participant| participant["id"].clone())
            .collect();
        assert_eq!(lanes, [json!(seed), json!(helper), json!(inner)]);
        // The std call: kept with its evidence and basis, hidden as incidental.
        assert_eq!(steps[0]["callEvidence"]["boundaryReason"], "external");
        assert_eq!(steps[0]["callEvidence"]["externalPackage"]["name"], "std");
        assert_eq!(
            steps[0]["callEvidence"]["semanticBasis"]["producerId"],
            graph_fixtures::PRODUCER
        );
        assert_eq!(steps[0]["hidden"], true);
        // The direct call names `helper` and carries its expanded body on `helper`'s lane.
        assert_eq!(steps[1]["target"], json!(helper));
        assert!(steps[1].get("lane").is_none());
        let body = steps[1]["children"].as_array().unwrap();
        assert_eq!(body.len(), 2, "{view}");
        assert!(body.iter().all(|step| step["lane"] == json!(helper)));
        assert_eq!(body[0]["target"], json!(inner));
        assert_eq!(body[0]["callEvidence"]["dispatch"], "direct");
        assert_eq!(body[1]["callEvidence"]["boundaryReason"], "external");
        assert_eq!(body[1]["hidden"], true);
        assert_ne!(body[0]["id"], steps[0]["id"]);
        assert_eq!(
            (view["hiddenSteps"].clone(), view["incidentalSteps"].clone()),
            (json!(2), json!(2))
        );
        // Show all: the same steps and evidence, none hidden.
        let (status, all) = sequence(true).await;
        assert_eq!(status, 200, "{all}");
        assert_eq!(
            (all["hiddenSteps"].clone(), all["incidentalSteps"].clone()),
            (json!(0), json!(2))
        );
        let all_steps = all["steps"].as_array().unwrap();
        assert_eq!(all_steps[0]["hidden"], false);
        assert_eq!(all_steps[1]["children"][1]["hidden"], false);
    }
}
