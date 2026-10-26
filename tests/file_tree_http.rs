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
    indexer::{IndexOptions, index_workspace_bundle},
    store::Store,
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
fn setup() -> (
    tempfile::TempDir,
    Store,
    Router,
    Arc<trellis::store::topology::WriterSession>,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("cwd");
    let workspace = root.join("sample");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();
    std::fs::write(root.join(".credentials"), "never return me").unwrap();
    std::fs::write(workspace.join("demo.js"), "function run() { save(); }").unwrap();
    let opts = IndexOptions::new(workspace.clone());
    let store = crate::common::open_store(&temp.path().join("state"), &workspace).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        index_workspace_bundle(&opts, store.root_id(), &cancel, |_| {}).unwrap();
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
    let state = http::new_with_browser_root(
        store.clone(),
        opts,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
        root,
    )
    .unwrap();
    // Offline snapshot test; the live reconciliation/read race is tracked in #111.
    state.retain_serving_session_without_tick_for_tests(session.clone());
    let app = http::router(state);
    (temp, store, app, session)
}
async fn call(app: &Router, path: &str) -> (u16, Value) {
    let req = Request::builder()
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    assert_eq!(res.headers()["cache-control"], "no-store");
    let status = res.status().as_u16();
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn cwd_metadata_maps_nested_index_without_content_or_mutation() {
    let (temp, store, app, _session) = setup();
    let before = store.status().unwrap();
    let (status, page) = call(&app, "/api/tree").await;
    assert_eq!(status, 200);
    assert!(page["root"].as_str().unwrap().ends_with("/cwd"));
    assert!(
        page["indexedWorkspace"]
            .as_str()
            .unwrap()
            .ends_with("/cwd/sample")
    );
    assert_eq!(page["items"][0]["name"], "sample");
    assert_eq!(
        page["revision"],
        serde_json::json!(store.status().unwrap().revision)
    );
    let rust = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "main.rs")
        .unwrap();
    assert!(rust["indexedPath"].is_null());
    assert!(rust["methodCount"].is_null());
    assert_eq!(rust["unindexedReason"], "Outside indexed workspace");
    assert!(page["items"][0].get("unindexedReason").is_none());
    assert!(!page.to_string().contains("never return me"));
    let (_, nested) = call(&app, "/api/tree?path=sample").await;
    assert_eq!(nested["items"][0]["path"], "sample/demo.js");
    assert_eq!(nested["items"][0]["indexedPath"], "demo.js");
    assert_eq!(nested["items"][0]["methodCount"], 1);
    assert!(nested["items"][0].get("unindexedReason").is_none());
    assert_eq!(
        call(
            &app,
            &format!(
                "/api/methods?path=demo.js&indexGeneration={}&indexRevision={}",
                store.status().unwrap().revision.index_generation,
                store.status().unwrap().revision.index_revision
            )
        )
        .await
        .0,
        200
    );
    std::fs::write(
        temp.path().join("cwd/sample/new.js"),
        "function newlyAdded() {}",
    )
    .unwrap();
    let (_, nested) = call(&app, "/api/tree?path=sample").await;
    assert!(nested["items"][1]["indexedPath"].is_null());
    assert_eq!(
        nested["items"][1]["unindexedReason"],
        "Not indexed yet (may be excluded or size-limited)"
    );
    assert_eq!(store.status().unwrap().revision, before.revision);
    let (_, page) = call(&app, "/api/tree?limit=1").await;
    assert_eq!(page["nextOffset"], 1);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
}
#[tokio::test]
async fn paths_errors_auth_and_guards() {
    let (_temp, _store, app, _session) = setup();
    for path in [
        "/api/tree?path=..",
        "/api/tree?path=%2Ftmp",
        "/api/tree?path=sample%2F..",
        "/api/tree?path=a%5Cb",
        "/api/tree?path=%00",
        "/api/tree?limit=201",
        "/api/tree?limit=0",
        "/api/tree?offset=10001",
        "/api/tree?extra=1",
    ] {
        assert_eq!(call(&app, path).await.0, 400, "{path}");
    }
    assert_eq!(call(&app, "/api/tree?path=main.rs").await.0, 422);
    assert_eq!(call(&app, "/api/tree?path=gone").await.0, 404);
    for (host, origin, token, expected) in [
        ("127.0.0.1:7331", None, None, 401),
        ("evil.test", None, Some(TOKEN), 403),
        ("127.0.0.1:7331", Some("http://evil.test"), Some(TOKEN), 403),
    ] {
        let mut request = Request::builder().uri("/api/tree").header("host", host);
        if let Some(origin) = origin {
            request = request.header("origin", origin);
        }
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        assert_eq!(
            app.clone()
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            expected
        );
    }
    let request = Request::builder()
        .uri("/api/tree")
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from(vec![0; 1024 * 1024 + 1]))
        .unwrap();
    assert_eq!(app.clone().oneshot(request).await.unwrap().status(), 413);
}
#[cfg(unix)]
#[tokio::test]
async fn symlink_listing_and_intermediate_escape_rejected() {
    let (temp, _store, app, _session) = setup();
    std::os::unix::fs::symlink(
        temp.path().join("cwd/sample"),
        temp.path().join("cwd/alias"),
    )
    .unwrap();
    let (_, page) = call(&app, "/api/tree").await;
    let link = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "alias")
        .unwrap();
    assert_eq!(link["kind"], "symlink");
    assert!(link["indexedPath"].is_null());
    for path in ["/api/tree?path=alias", "/api/tree?path=alias/child"] {
        assert!([403, 422].contains(&call(&app, path).await.0));
    }
}

#[tokio::test]
async fn reasons_distinguish_languages_pending_files_and_workspace_boundary() {
    let (temp, _store, app, _session) = setup();
    let cases = [
        (
            "main.rs",
            "Not indexed yet (may be excluded or size-limited)",
        ),
        ("main.ts", "TypeScript indexing not supported yet"),
        ("main.tsx", "TypeScript indexing not supported yet"),
        ("README.md", "Unsupported source type"),
        ("no-extension", "Unsupported source type"),
        (
            "new.js",
            "Not indexed yet (may be excluded or size-limited)",
        ),
        (
            "new.mjs",
            "Not indexed yet (may be excluded or size-limited)",
        ),
        (
            "new.cjs",
            "Not indexed yet (may be excluded or size-limited)",
        ),
    ];
    for (name, _) in cases {
        std::fs::write(
            temp.path().join("cwd/sample").join(name),
            "never return content",
        )
        .unwrap();
    }
    let (status, page) = call(&app, "/api/tree?path=sample").await;
    assert_eq!(status, 200);
    for (name, reason) in cases {
        let entry = page["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["name"] == name)
            .unwrap();
        assert_eq!(entry["unindexedReason"], reason, "{name}");
        assert!(entry["indexedPath"].is_null());
        assert!(entry["methodCount"].is_null());
    }
    assert!(!page.to_string().contains("never return content"));
    // A sibling sharing the workspace name prefix must still be outside it.
    std::fs::create_dir(temp.path().join("cwd/sample-other")).unwrap();
    for (name, _) in cases {
        std::fs::write(temp.path().join("cwd/sample-other").join(name), "").unwrap();
    }
    let (_, page) = call(&app, "/api/tree?path=sample-other").await;
    for entry in page["items"].as_array().unwrap() {
        assert_eq!(entry["unindexedReason"], "Outside indexed workspace");
    }
}
