#![cfg(feature = "test-support")]
//! Offline catalog and cached-source sequence API checks.
mod common;
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

#[path = "common/graph_fixtures.rs"]
mod graph_fixtures;

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
