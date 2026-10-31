#![cfg(feature = "test-support")]
use crate::common;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use tower::ServiceExt;
use trellis::{
    daemon::registry::{CheckoutOptions, CheckoutRegistry},
    http::{self, ProvisionedBrowser},
    indexer::{IndexOptions, index_workspace_bundle},
    model::IndexPin,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity, WriterSession},
    },
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
async fn call(
    app: &Router,
    method: &str,
    path: &str,
    body: Value,
    auth: bool,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("content-type", "application/json");
    if auth {
        request = request.header("authorization", format!("Bearer {TOKEN}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    assert_eq!(headers["cache-control"], "no-store");
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (
        status,
        headers,
        serde_json::from_slice(&body).unwrap_or(Value::Null),
    )
}
fn fixture() -> (tempfile::TempDir, Store, Router, Arc<WriterSession>) {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("a.js"),
        "function chosen() {}\nchosen(); chosen();\n",
    )
    .unwrap();
    let store = common::open_store(&temp.path().join("state"), &workspace).unwrap();
    let options = IndexOptions::new(workspace);
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
    state.retain_serving_session_without_tick_for_tests(session.clone());
    (temp, store, http::router(state), session)
}
fn outline_path(prefix: &str, pin: IndexPin) -> String {
    format!(
        "{prefix}/symbols?path=a.js&indexGeneration={}&indexRevision={}",
        pin.index_generation, pin.index_revision
    )
}
#[tokio::test]
async fn fixture_routes_require_full_pins_closed_selectors_auth_and_cached_only_evidence() {
    let (_temp, store, app, _session) = fixture();
    let pin = store.status().unwrap().revision;
    let path = outline_path("/api", pin);
    assert_eq!(call(&app, "GET", &path, Value::Null, false).await.0, 401);
    let (status, _, outline) = call(&app, "GET", &path, Value::Null, true).await;
    assert_eq!(status, 200, "{outline}");
    let id = outline["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["symbol"]["name"] == "chosen")
        .unwrap()["symbol"]["id"]
        .clone();
    let body = json!({"expectedRevision":pin,"symbolId":id,"limit":1});
    assert_eq!(
        call(&app, "POST", "/api/syntax-matches", body.clone(), false)
            .await
            .0,
        401
    );
    let (status, _, matches) = call(&app, "POST", "/api/syntax-matches", body, true).await;
    assert_eq!(status, 200, "{matches}");
    assert_eq!(matches["evidenceKind"], "syntaxMatches");
    assert_eq!(matches["items"].as_array().unwrap().len(), 1);
    assert!(matches["continuation"].is_object());
    assert_eq!(matches["truncated"], true);
    for path in [
        "/api/symbols?path=a.js".to_owned(),
        format!("{path}&extra=1"),
        format!("{path}&limit=201"),
        path.replace("path=a.js", "path=../a.js"),
        format!(
            "/api/symbols?path=a.js&indexGeneration={}",
            pin.index_generation
        ),
    ] {
        assert_eq!(
            call(&app, "GET", &path, Value::Null, true).await.0,
            400,
            "{path}"
        );
    }
    for body in [
        json!({"symbolId":id}),
        json!({"expectedRevision":pin,"symbolId":id,"extra":1}),
        json!({"expectedRevision":pin,"symbolId":id,"limit":201}),
        json!({"expectedRevision":pin,"symbolId":id,"continuation":{}}),
    ] {
        assert_eq!(
            call(&app, "POST", "/api/syntax-matches", body, true)
                .await
                .0,
            400
        );
    }
    let mut wrong_cursor = matches["continuation"].clone();
    wrong_cursor["path"] = json!("missing.js");
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/syntax-matches",
            json!({"expectedRevision":pin,"symbolId":id,"continuation":wrong_cursor}),
            true
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/syntax-matches",
            json!({"expectedRevision":pin,"symbolId":"missing"}),
            true
        )
        .await
        .0,
        404
    );
    let wrong_pin = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        ..pin
    };
    let (status, _, error) = call(
        &app,
        "POST",
        "/api/syntax-matches",
        json!({"expectedRevision":wrong_pin,"symbolId":id}),
        true,
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(error["error"]["code"], "revision_conflict");
}

#[tokio::test]
async fn production_selected_routes_admit_evidence_and_refuse_unselected_direct_routes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("checkout");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function chosen() {}\nchosen();\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry
        .register(&identity, CheckoutOptions(json!({})))
        .unwrap();
    let registry = Arc::new(tokio::sync::Mutex::new(registry));
    let browser = ProvisionedBrowser::new(
        registry.clone(),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let app = browser.router();
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    let status_path = format!("{prefix}/status");
    let status = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (status, _, body) = call(&app, "GET", &status_path, Value::Null, true).await;
            if status == StatusCode::OK {
                break body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let pin: IndexPin = serde_json::from_value(status["revision"].clone()).unwrap();
    let (status, headers, outline) =
        call(&app, "GET", &outline_path(&prefix, pin), Value::Null, true).await;
    assert_eq!(status, 200, "{outline}");
    assert_eq!(
        headers["x-trellis-workspace"],
        identity.root.to_str().unwrap()
    );
    let id = outline["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["symbol"]["name"] == "chosen")
        .unwrap()["symbol"]["id"]
        .clone();
    let body = json!({"expectedRevision":pin,"symbolId":id});
    let (status, headers, matches) = call(
        &app,
        "POST",
        &format!("{prefix}/syntax-matches"),
        body.clone(),
        true,
    )
    .await;
    assert_eq!(status, 200, "{matches}");
    assert_eq!(matches["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        headers["x-trellis-workspace"],
        identity.root.to_str().unwrap()
    );
    for (method, path, body) in [
        ("GET", outline_path("/api", pin), Value::Null),
        ("POST", "/api/syntax-matches".into(), body),
    ] {
        assert_eq!(call(&app, method, &path, body, true).await.0, 404);
    }
    for asset in ["/files.js", "/files.css"] {
        let (status, headers, _) = call(&app, "GET", asset, Value::Null, false).await;
        assert_eq!(status, 200);
        assert!(
            headers["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("script-src 'self'")
        );
    }
}

#[tokio::test]
async fn production_new_evidence_routes_keep_before_finish_and_final_selected_root_fences() {
    for (stage, suffix) in [
        ("before_read_finish", "symbols"),
        ("after_handler", "symbols"),
        ("before_read_finish", "syntax-matches"),
        ("after_handler", "syntax-matches"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.js"), "function chosen() {}\nchosen();\n").unwrap();
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let roots =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let mut registry = CheckoutRegistry::with_roots(roots);
        registry
            .register(&identity, CheckoutOptions(json!({})))
            .unwrap();
        let registry = Arc::new(tokio::sync::Mutex::new(registry));
        let browser =
            ProvisionedBrowser::new(registry, TOKEN.into(), "127.0.0.1:7331".parse().unwrap())
                .unwrap();
        let app = browser.clone().router();
        let prefix = format!("/api/checkouts/{}", identity.root_key);
        let status = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                let (status, _, body) =
                    call(&app, "GET", &format!("{prefix}/status"), Value::Null, true).await;
                if status == StatusCode::OK {
                    break body;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let pin: IndexPin = serde_json::from_value(status["revision"].clone()).unwrap();
        let (_, _, outline) =
            call(&app, "GET", &outline_path(&prefix, pin), Value::Null, true).await;
        let id = outline["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["symbol"]["name"] == "chosen")
            .unwrap()["symbol"]["id"]
            .clone();
        let changed = Arc::new(AtomicBool::new(false));
        let changed_hook = changed.clone();
        let source = root.clone();
        let moved = temp.path().join("moved");
        browser.set_selected_response_hook_for_tests(Arc::new(move |at| {
            if at == stage && !changed_hook.swap(true, Ordering::AcqRel) {
                std::fs::rename(&source, &moved).unwrap();
            }
        }));
        let (method, path, body) = if suffix == "symbols" {
            ("GET", outline_path(&prefix, pin), Value::Null)
        } else {
            (
                "POST",
                format!("{prefix}/syntax-matches"),
                json!({"expectedRevision":pin,"symbolId":id}),
            )
        };
        let (status, _, error) = call(&app, method, &path, body, true).await;
        assert!(changed.load(Ordering::Acquire), "{stage}/{suffix}");
        assert_eq!(status, 409, "{stage}/{suffix}: {error}");
        assert_eq!(error["error"]["code"], "root_changed");
        assert!(error.get("items").is_none());
    }
}
