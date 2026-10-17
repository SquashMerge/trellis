#![cfg(feature = "test-support")]
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use std::{fs, sync::Arc, time::Duration};
use tower::ServiceExt;
use trellis::{
    daemon::registry::{CheckoutOptions, CheckoutRegistry},
    http::ProvisionedBrowser,
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
async fn request(app: axum::Router, key: &str, query: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/checkouts/{key}/metrics{query}"))
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn selected_metrics_projection_and_validation() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("sample.js"), "function sample(x) { return x; }\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let roots =
        TopologyRoots::isolated_for_tests(base.path().join("cache"), base.path().join("data"));
    let store = Store::open(
        roots.clone(),
        WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .attach_marker()
            .unwrap(),
    )
    .unwrap();
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leader = trellis::index_coordinator::establish_serving_session(
        &store,
        Some(&trellis::indexer::IndexOptions::new(root.clone())),
        &cancel,
    )
    .unwrap();
    leader.verify().unwrap();
    drop(leader);
    drop(store);
    let mut registry = CheckoutRegistry::with_roots(roots);
    registry
        .register(&identity, CheckoutOptions(serde_json::json!({})))
        .unwrap();
    registry.attach_launch(1, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response()
                && let Ok(payload) =
                    response.metrics_at(&trellis::store::MetricsOptions::default(), &mut || Ok(()))
                && payload.files.len() == 1
                && response.finish(()).is_ok()
                && !catching_up
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    let app = ProvisionedBrowser::new(
        Arc::new(tokio::sync::Mutex::new(registry)),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap()
    .router();
    let key = &identity.root_key;
    let (status, response) = request(app.clone(), key, "").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["catchingUp"], false);
    assert_eq!(response["payload"]["metricsVersion"], "metrics:v1");
    assert_eq!(response["payload"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(response["payload"]["declarations"], serde_json::json!([]));
    let (repeat_status, repeated) = request(app.clone(), key, "").await;
    assert_eq!(repeat_status, StatusCode::OK);
    assert_eq!(
        response["payload"], repeated["payload"],
        "memoized read must be deterministic"
    );
    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/checkouts/{key}/metrics"))
                .header("host", "127.0.0.1:7331")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    let generation = response["payload"]["indexGeneration"].as_str().unwrap();
    let revision = response["payload"]["indexRevision"].as_u64().unwrap();
    let source = response["payload"]["files"][0]["sourceSetId"]
        .as_str()
        .unwrap();
    let (status, file) = request(app.clone(), key, &format!("?scope=file&sourceSetId={source}&language=javascript&path=sample.js&indexGeneration={generation}&indexRevision={revision}&metrics=loc,fanOut")).await;
    assert_eq!(status, StatusCode::OK, "{file}");
    assert_eq!(file["payload"]["declarations"].as_array().unwrap().len(), 1);
    assert_eq!(
        file["payload"]["selectedMetrics"],
        serde_json::json!(["loc", "fanOut"])
    );
    for query in [
        "?scope=file",
        "?metrics=",
        "?metrics=loc,loc",
        "?wat=1",
        "?scope=revision&scope=file",
        "?indexGeneration=bad",
        "?scope=file&sourceSetId=x&language=javascript&path=../a",
    ] {
        let (status, _) = request(app.clone(), key, query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
    let (status, _) = request(
        app.clone(),
        key,
        &format!(
            "?indexGeneration={generation}&indexRevision={}",
            revision + 100
        ),
    )
    .await;
    assert_ne!(status, StatusCode::OK);
    let (status, _) = request(
        app.clone(),
        key,
        &format!("?indexGeneration=00000000-0000-0000-0000-000000000000&indexRevision={revision}"),
    )
    .await;
    assert_ne!(
        status,
        StatusCode::OK,
        "wrong generation must never fall back"
    );
    let (status, missing) = request(
        app.clone(),
        key,
        &format!("?scope=file&sourceSetId={source}&language=javascript&path=missing.js"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    let sid = file["payload"]["declarations"][0]["sid"].as_str().unwrap();
    let (status, declaration) = request(
        app.clone(),
        key,
        &format!("?scope=declaration&sourceSetId={source}&path=sample.js&sid={sid}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{declaration}");
    assert_eq!(declaration["payload"]["files"], serde_json::json!([]));
    assert_eq!(
        declaration["payload"]["declarations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let (status, directory) = request(
        app.clone(),
        key,
        &format!("?scope=directory&sourceSetId={source}&path="),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{directory}");
    assert_eq!(
        directory["payload"]["directories"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
