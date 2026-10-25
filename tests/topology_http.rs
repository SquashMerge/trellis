#![cfg(feature = "test-support")]
mod common;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::Value;
use std::sync::{Arc, atomic::AtomicBool};
use tower::ServiceExt;
use trellis::{
    http,
    indexer::{IndexOptions, index_workspace},
    model::{CancelFlag, Graph},
    store::{Store, topology::WriterSession},
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
fn fixture() -> (
    tempfile::TempDir,
    Store,
    Graph,
    Router,
    String,
    Arc<WriterSession>,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.js"),
        "function seed() { other(); } function other() {}",
    )
    .unwrap();
    let options = IndexOptions::new(root.clone());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let graph = index_workspace(&options, &cancel, |_| {}).unwrap();
    let seed = graph
        .nodes
        .iter()
        .find(|node| node.name == "seed")
        .unwrap()
        .id
        .clone();
    let store = crate::common::open_store(&temp.path().join("state"), &root).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    let app = http::router(state);
    (temp, store, graph, app, seed, session)
}
async fn call(app: &Router, method: &str, path: &str, body: Value) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let code = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
#[tokio::test]
async fn workspace_root_changed() {
    let (temp, store, _graph, app, _seed, _session) = fixture();
    let original = temp.path().join("workspace");
    let moved = temp.path().join("moved");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    for (method, path, body) in [
        ("GET", "/api/status", Value::Null),
        ("GET", "/api/tree", Value::Null),
        ("GET", "/api/files", Value::Null),
        ("GET", "/api/classes", Value::Null),
    ] {
        let (code, result) = call(&app, method, path, body).await;
        assert_eq!(code, 409, "{path}: {result}");
        assert_eq!(result["error"]["code"], "root_changed");
    }
    assert!(store.status().is_err());
}
