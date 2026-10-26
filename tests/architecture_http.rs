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
                .uri(format!("/api/checkouts/{key}/architecture{query}"))
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
async fn fixture() -> (
    tempfile::TempDir,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
    ProvisionedBrowser,
    String,
) {
    fixture_with_roles(false).await
}
async fn fixture_with_roles(
    role_fixture: bool,
) -> (
    tempfile::TempDir,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
    ProvisionedBrowser,
    String,
) {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("checkout");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("sample.js"), "function sample(x) { return x; }\n").unwrap();
    fs::create_dir(root.join("deep")).unwrap();
    fs::write(
        root.join("deep/extra.js"),
        "function extra() { return 2; }\n",
    )
    .unwrap();
    fs::write(root.join("empty.js"), "// no callable\n").unwrap();
    fs::create_dir(root.join("zero")).unwrap();
    fs::write(root.join("zero/empty.js"), "// zero-callable directory\n").unwrap();
    if role_fixture {
        fs::create_dir(root.join("src")).unwrap();
        fs::write(
            root.join("src/roles.rs"),
            r#"fn production() { crate::target::run(); }
#[cfg(feature = "test-support")] fn support() { crate::target::run(); }
#[cfg(test)] mod tests { fn check() { crate::target::run(); } }
"#,
        )
        .unwrap();
        fs::write(root.join("src/target.rs"), "pub fn run() {}\n").unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(
            root.join("tests/only.rs"),
            "fn only() { crate::target::run(); }\n",
        )
        .unwrap();
    }
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
    {
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                if let Ok((response, catching_up)) = runtime.evidence_response()
                    && response.status().is_ok()
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
    }
    let browser = ProvisionedBrowser::new(
        Arc::new(tokio::sync::Mutex::new(registry)),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let key = identity.root_key;
    (base, runtime, browser, key)
}

fn write_rules(base: &tempfile::TempDir, text: &str) {
    let path = base.path().join("checkout/.trellis");
    fs::create_dir_all(&path).unwrap();
    fs::write(path.join("rules.toml"), text).unwrap();
}
const RULES: &str = "version=1\n[layers.app]\norder=1\npaths=['**/*.js']";
#[tokio::test]
async fn architecture_route_pin_aggregate_setup_and_located_diagnostics() {
    let (base, _runtime, browser, key) = fixture().await;
    let app = browser.clone().router();
    let (status, missing) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{missing}");
    assert_eq!(missing["payload"]["state"], "missing_rules");
    assert_eq!(missing["payload"]["aggregate"], "max");
    assert_eq!(missing["payload"]["role"], "all");
    assert!(missing["payload"].get("metricsScope").is_none());
    let generation = missing["payload"]["indexGeneration"].as_str().unwrap();
    let revision = missing["payload"]["indexRevision"].as_u64().unwrap();
    write_rules(&base, RULES);
    let (status, ready) = request(
        app.clone(),
        &key,
        &format!("?indexGeneration={generation}&indexRevision={revision}&aggregate=mean"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["payload"]["aggregate"], "mean");
    assert_eq!(ready["payload"]["modules"].as_array().unwrap().len(), 4);
    for query in [
        format!("?indexGeneration={generation}"),
        format!("?indexRevision={revision}"),
        "?aggregate=avg".into(),
        "?aggregate=sum&aggregate=max".into(),
        "?role=".into(),
        "?role=unknown".into(),
        "?role=Production".into(),
        "?role=all&role=test".into(),
        "?role=all&%72ole=production".into(),
        "?scope=revision".into(),
        "?metrics=cyclomatic".into(),
        "?unexpected=x".into(),
    ] {
        let (status, error) = request(app.clone(), &key, &query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {error}");
    }
    let (status, stale) = request(
        app.clone(),
        &key,
        &format!(
            "?indexGeneration={generation}&indexRevision={}",
            revision + 1
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{stale}");
    write_rules(&base, "version=[");
    let (status, invalid) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{invalid}");
    assert_eq!(invalid["error"]["code"], "invalid_rules");
    for field in [
        "kind",
        "path",
        "range",
        "line",
        "column",
        "attribution",
        "message",
    ] {
        assert!(!invalid["error"][field].is_null(), "{field}: {invalid}");
    }
    assert!(invalid.get("payload").is_none());
}
#[tokio::test]
async fn architecture_role_filters_metrics_without_changing_reference_evidence() {
    let (base, _runtime, browser, key) = fixture_with_roles(true).await;
    write_rules(
        &base,
        "version=1\n[layers.app]\norder=2\npaths=['src/roles.rs','tests/**']\n[layers.target]\norder=1\npaths=['src/target.rs']",
    );
    let app = browser.router();
    let (status, omitted) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::OK, "{omitted}");
    let baseline = &omitted["payload"];
    assert_eq!(baseline["role"], "all");
    assert!(
        !baseline["references"]["production"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        !baseline["references"]["tests"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let generation = baseline["indexGeneration"].as_str().unwrap();
    let revision = baseline["indexRevision"].as_u64().unwrap();
    let pin = format!("indexGeneration={generation}&indexRevision={revision}");
    for (role, count) in [("production", 2), ("all", 3), ("test", 1)] {
        let (status, selected) = request(app.clone(), &key, &format!("?role={role}&{pin}")).await;
        assert_eq!(status, StatusCode::OK, "{selected}");
        let payload = &selected["payload"];
        assert_eq!(payload["role"], role);
        assert!(payload.get("metricsScope").is_none());
        for field in [
            "indexGeneration",
            "indexRevision",
            "aggregate",
            "references",
            "edges",
            "rules",
            "layers",
        ] {
            assert_eq!(payload[field], baseline[field], "{role}: {field}");
        }
        assert_eq!(
            payload["modules"].as_array().unwrap().len(),
            baseline["modules"].as_array().unwrap().len()
        );
        let mixed = payload["modules"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["identity"]["path"] == "src/roles.rs")
            .unwrap();
        assert_eq!(mixed["declarationCount"], count);
        assert_eq!(
            mixed["roleCounts"],
            serde_json::json!({"production":2,"test":1})
        );
        assert_eq!(mixed["physicalLines"], 3);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/checkouts/{key}/metrics?role={role}&aggregate=max&{pin}"
                    ))
                    .header("host", "127.0.0.1:7331")
                    .header("authorization", format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap();
        let metrics: Value = serde_json::from_slice(&bytes).unwrap();
        for module in payload["modules"].as_array().unwrap() {
            let file = metrics["payload"]["files"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| {
                    row["sourceSetId"] == module["identity"]["sourceSetId"]
                        && row["language"] == module["identity"]["language"]
                        && row["path"] == module["identity"]["path"]
                })
                .unwrap();
            for field in ["metrics", "declarationCount", "roleCounts"] {
                assert_eq!(module[field], file[field], "{role}: {field}");
            }
        }
        if role == "production" {
            let test_only = payload["modules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["identity"]["path"] == "tests/only.rs")
                .unwrap();
            assert_eq!(test_only["declarationCount"], 0);
            assert_eq!(
                test_only["roleCounts"],
                serde_json::json!({"production":0,"test":1})
            );
            assert!(test_only["metrics"]["cyclomatic"].is_null());
        }
    }
}

#[tokio::test]
async fn architecture_bounds_cancellation_deadline_and_final_root_fences() {
    let (base, _runtime, browser, key) = fixture().await;
    write_rules(&base, RULES);
    browser.set_metrics_controls_for_tests(20, None, false);
    let (status, bounded) = request(browser.clone().router(), &key, "").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{bounded}");
    assert_eq!(bounded["error"]["code"], "response_too_large");
    browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, true);
    let (_status, cancelled) = request(browser.clone().router(), &key, "").await;
    assert_eq!(cancelled["error"]["code"], "cancelled");
    browser.set_metrics_controls_for_tests(64 * 1024 * 1024, Some(Duration::from_millis(1)), false);
    browser.set_metrics_clock_skew_for_tests(1001);
    let (_status, expired) = request(browser.clone().router(), &key, "").await;
    assert_eq!(expired["error"]["code"], "deadline_exceeded");
    browser.set_metrics_clock_skew_for_tests(0);
    browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, false);
    // Located errors also pass the owned final read/root fence.
    write_rules(&base, "version=[");
    let root = base.path().join("checkout");
    let moved = base.path().join("moved");
    let once = std::sync::atomic::AtomicBool::new(true);
    browser.set_selected_response_hook_for_tests(Arc::new(move |stage| {
        if stage == "before_read_finish" && once.swap(false, std::sync::atomic::Ordering::AcqRel) {
            fs::rename(&root, &moved).unwrap();
        }
    }));
    let (status, changed) = request(browser.clone().router(), &key, "").await;
    assert_eq!(status, StatusCode::CONFLICT, "{changed}");
    assert_eq!(changed["error"]["code"], "root_changed");
    assert!(changed["error"].get("kind").is_none());
}
