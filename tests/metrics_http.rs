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
async fn fixture() -> (
    tempfile::TempDir,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
    ProvisionedBrowser,
    String,
) {
    fixture_with_startup_hook(None, true, false).await
}
async fn fixture_with_startup_hook(
    startup_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    wait_ready: bool,
    parser_fixture: bool,
) -> (
    tempfile::TempDir,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
    ProvisionedBrowser,
    String,
) {
    fixture_with_identity(startup_hook, wait_ready, parser_fixture, false).await
}
async fn fixture_with_identity(
    startup_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    wait_ready: bool,
    parser_fixture: bool,
    git: bool,
) -> (
    tempfile::TempDir,
    Arc<trellis::daemon::registry::CheckoutRuntime>,
    ProvisionedBrowser,
    String,
) {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("checkout");
    fs::create_dir(&root).unwrap();
    if git {
        fs::create_dir(root.join(".git")).unwrap();
    }
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
    if parser_fixture {
        let mut source = String::from("function parseMany() {\n");
        for i in 0..4000 {
            source.push_str(&format!("  const n{i} = {i};\n"));
        }
        source.push_str("  return 1;\n}\n");
        fs::write(root.join("parser.js"), source).unwrap();
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
    if let Some(hook) = startup_hook {
        registry.set_next_startup_hook_for_tests(hook);
    }
    let runtime = registry.activate(&identity.root_key).unwrap();
    if wait_ready {
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
#[tokio::test]
async fn selected_metrics_top_cognitive_validation_and_git_identity() {
    for git in [false, true] {
        let (_base, _runtime, browser, key) = fixture_with_identity(None, true, false, git).await;
        let app = browser.clone().router();
        let (status, ordinary) = request(app.clone(), &key, "").await;
        assert_eq!(status, StatusCode::OK, "{ordinary}");
        assert!(ordinary["payload"].get("topCognitive").is_none());
        let payload = &ordinary["payload"];
        let source = payload["files"][0]["sourceSetId"].as_str().unwrap();
        if git {
            let identity = source.strip_prefix("source-set:v1:").unwrap();
            assert_eq!(
                uuid::Uuid::parse_str(identity).unwrap().to_string(),
                identity
            );
        } else {
            assert!(source.starts_with("source-set:v1:path-"));
        }
        let generation = payload["indexGeneration"].as_str().unwrap();
        let revision = payload["indexRevision"].as_u64().unwrap();
        let pin = format!("indexGeneration={generation}&indexRevision={revision}");
        let query = format!(
            "?scope=directory&sourceSetId={source}&path=&aggregate=mean&metrics=loc&topCognitive=1&{pin}"
        );
        let (status, directory) = request(app.clone(), &key, &query).await;
        assert_eq!(status, StatusCode::OK, "{directory}");
        assert_eq!(directory["payload"]["declarations"], serde_json::json!([]));
        let top = &directory["payload"]["topCognitive"];
        assert_eq!(top.as_array().unwrap().len(), 1);
        assert_eq!(top[0]["name"], "extra");
        assert_eq!(top[0]["kind"], "function");
        assert_eq!(top[0]["path"], "deep/extra.js");
        assert_eq!(top[0]["sourceSetId"], source);
        assert_eq!(
            top[0]["metrics"],
            serde_json::json!({"cognitive":0,"cyclomatic":1,"maxNesting":0})
        );
        browser.set_metrics_controls_for_tests(
            serde_json::to_vec(&directory).unwrap().len() - 1,
            None,
            false,
        );
        let (status, oversized) = request(app.clone(), &key, &query).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{oversized}");
        assert_eq!(oversized["error"]["code"], "response_too_large");
        browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, true);
        let (status, cancelled) = request(app.clone(), &key, &query).await;
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{cancelled}");
        assert_eq!(cancelled["error"]["code"], "cancelled");
        browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, false);
        let (status, file) = request(app.clone(), &key, &format!("?scope=file&sourceSetId={source}&language=javascript&path=sample.js&topCognitive=10&{pin}")).await;
        assert_eq!(status, StatusCode::OK, "{file}");
        assert_eq!(file["payload"]["topCognitive"][0]["name"], "sample");
        assert_eq!(
            file["payload"]["topCognitive"][0]["sid"],
            file["payload"]["declarations"][0]["sid"]
        );
        assert_eq!(
            file["payload"]["topCognitive"][0]["range"],
            file["payload"]["declarations"][0]["range"]
        );
        assert!(file["payload"]["declarations"][0].get("name").is_none());
        let (status, wrong_pin) = request(
            app.clone(),
            &key,
            &format!(
                "?topCognitive=3&indexGeneration={generation}&indexRevision={}",
                revision + 1
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{wrong_pin}");
        assert_eq!(wrong_pin["error"]["code"], "revision_conflict");
        for count in [
            "",
            "0",
            "11",
            "-1",
            "1.0",
            "%2B1",
            "99999999999999999999999999",
        ] {
            let (status, error) =
                request(app.clone(), &key, &format!("?topCognitive={count}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{error}");
            assert_eq!(error["error"]["code"], "invalid_request");
        }
        let (status, _) = request(app.clone(), &key, "?topCognitive=1&topCognitive=2").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        for identity in [
            "SOURCE-SET:v1:path-0000000000000000000000000000000000000000000000000000000000000000",
            "source-set:v1:550E8400-E29B-41D4-A716-446655440000",
            "source-set:v1:550e8400e29b41d4a716446655440000",
            "source-set:v1:not-a-uuid",
        ] {
            let (status, _) = request(
                app.clone(),
                &key,
                &format!("?scope=directory&sourceSetId={identity}&path="),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{identity}");
        }
    }
}

#[tokio::test]
async fn selected_metrics_projection_and_validation() {
    let (_base, _runtime, browser, key) = fixture().await;
    let admitted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let admissions = admitted.clone();
    browser.set_selected_response_hook_for_tests(Arc::new(move |stage| {
        if stage == "before_work" {
            admissions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }));
    let app = browser.router();
    let key = &key;
    let (status, response) = request(app.clone(), key, "").await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let raw = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/checkouts/{key}/metrics"))
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(raw.headers()["X-Trellis-Catching-Up"], "false");
    let raw_bytes = to_bytes(raw.into_body(), 64 * 1024 * 1024).await.unwrap();
    let pinned_uri = format!(
        "/api/checkouts/{key}/metrics?indexGeneration={}&indexRevision={}",
        response["payload"]["indexGeneration"].as_str().unwrap(),
        response["payload"]["indexRevision"].as_u64().unwrap()
    );
    let pinned = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(&pinned_uri)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pinned.status(), StatusCode::OK);
    assert_eq!(
        raw_bytes,
        to_bytes(pinned.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap()
    );
    assert_eq!(response["catchingUp"], false);
    assert_eq!(response["payload"]["metricsVersion"], "metrics:v1");
    assert_eq!(response["payload"]["files"].as_array().unwrap().len(), 4);
    assert_eq!(response["payload"]["declarations"], serde_json::json!([]));
    assert_eq!(
        response["payload"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["deep/extra.js", "empty.js", "sample.js", "zero/empty.js"]
    );
    assert_eq!(
        response["payload"]["directories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["", "deep", "zero"]
    );
    assert_eq!(response["payload"]["directories"][0]["declarationCount"], 2);
    assert_eq!(response["payload"]["directories"][2]["path"], "zero");
    assert_eq!(response["payload"]["directories"][2]["declarationCount"], 0);
    assert!(response["payload"]["directories"][2]["metrics"]["loc"].is_null());
    assert_eq!(
        response["payload"]["directories"][0]["metrics"]["cyclomatic"],
        2
    );
    // Body-only leaves are `return` and one operand per function: V = 2 each.
    assert_eq!(
        response["payload"]["directories"][0]["metrics"]["halstead"]["volume"],
        4.0
    );
    let (status, max) = request(app.clone(), key, "?aggregate=max").await;
    assert_eq!(status, StatusCode::OK, "{max}");
    assert_eq!(max["payload"]["directories"][0]["declarationCount"], 2);
    assert_eq!(
        max["payload"]["directories"][0]["metrics"]["halstead"]["volume"],
        2.0
    );
    assert_eq!(max["payload"]["directories"][0]["metrics"]["parameters"], 1);
    assert_eq!(
        max["payload"]["directories"][1]["metrics"]["halstead"]["volume"],
        2.0
    );
    assert!(max["payload"]["directories"][2]["metrics"]["halstead"]["volume"].is_null());
    let (status, mean) = request(app.clone(), key, "?aggregate=mean").await;
    assert_eq!(status, StatusCode::OK, "{mean}");
    assert_eq!(
        mean["payload"]["directories"][0]["metrics"]["halstead"]["volume"],
        2.0
    );
    assert_eq!(
        mean["payload"]["directories"][0]["metrics"]["parameters"],
        0.5
    );
    assert_eq!(mean["payload"]["directories"][1]["metrics"]["loc"], 1.0);
    assert!(mean["payload"]["directories"][2]["metrics"]["loc"].is_null());
    assert_eq!(
        response["payload"]["directories"][1]["metrics"]["exits"]["returns"],
        1
    );
    assert_eq!(response["payload"]["files"][1]["declarationCount"], 0);
    assert!(response["payload"]["files"][1]["metrics"]["loc"].is_null());
    assert_eq!(response["payload"]["files"][2]["metrics"]["loc"], 1);
    assert_eq!(response["payload"]["files"][2]["metrics"]["cyclomatic"], 1);
    assert_eq!(response["payload"]["files"][2]["metrics"]["parameters"], 1);
    assert_eq!(
        response["payload"]["files"][2]["metrics"]["halstead"]["volume"],
        2.0
    );

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
    for (header, value) in [("host", "evil.invalid"), ("origin", "http://evil.invalid")] {
        let mut builder = Request::builder()
            .uri(format!("/api/checkouts/{key}/metrics"))
            .header("host", "127.0.0.1:7331")
            .header("authorization", format!("Bearer {TOKEN}"));
        builder = builder.header(header, value);
        assert_eq!(
            app.clone()
                .oneshot(builder.body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    let (wrong_root, root_error) = request(app.clone(), &"0".repeat(64), "").await;
    assert_eq!(wrong_root, StatusCode::BAD_REQUEST, "{root_error}");
    assert_eq!(root_error["error"]["code"], "workspace_selection_failed");
    assert_eq!(root_error["error"]["reason"], "not_checkout");
    let generation = response["payload"]["indexGeneration"].as_str().unwrap();
    let revision = response["payload"]["indexRevision"].as_u64().unwrap();
    let source = response["payload"]["files"][0]["sourceSetId"]
        .as_str()
        .unwrap();
    let (status, file) = request(app.clone(), key, &format!("?scope=file&sourceSetId={source}&language=javascript&path=sample.js&indexGeneration={generation}&indexRevision={revision}&metrics=loc,fanOut")).await;
    assert_eq!(status, StatusCode::OK, "{file}");
    assert_eq!(file["payload"]["declarations"].as_array().unwrap().len(), 1);
    assert_eq!(
        file["payload"]["declarations"][0]["metrics"],
        serde_json::json!({"loc":1,"fanOut":0})
    );
    assert_eq!(
        file["payload"]["selectedMetrics"],
        serde_json::json!(["loc", "fanOut"])
    );
    let (status, ordered) = request(app.clone(), key, "?metrics=parameters,fanOut,loc").await;
    assert_eq!(status, StatusCode::OK, "{ordered}");
    assert_eq!(
        ordered["payload"]["selectedMetrics"],
        serde_json::json!(["loc", "fanOut", "parameters"])
    );
    let before_invalid = admitted.load(std::sync::atomic::Ordering::Relaxed);
    for query in [
        "?scope=file",
        "?metrics=",
        "?metrics=loc,loc",
        "?metrics=unknown",
        "?",
        "?scope=revision&sourceSetId=source-set:v1:path-0000000000000000000000000000000000000000000000000000000000000000",
        "?scope=file&sourceSetId=source-set:v1:garbage&language=javascript&path=sample.js",
        "?wat=1",
        "?scope=revision&scope=file",
        "?indexGeneration=bad",
        "?scope=file&sourceSetId=x&language=javascript&path=../a",
        "?scope=declaration&sourceSetId=source-set:v1:garbage&path=sample.js&sid=sid:v1:not-hex",
    ] {
        let (status, _) = request(app.clone(), key, query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
    for query in [
        format!("?scope=declaration&sourceSetId={source}&path=sample.js&sid=sid:v1:not-hex"),
        format!(
            "?scope=file&sourceSetId={source}&language=javascript&path=sample.js&sid=sid:v1:00000000000000000000000000000000"
        ),
        format!(
            "?scope=declaration&sourceSetId={source}&language=javascript&path=sample.js&sid=sid:v1:00000000000000000000000000000000"
        ),
    ] {
        let (status, value) = request(app.clone(), key, &query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}: {value}");
        assert_eq!(value["error"]["code"], "invalid_request");
    }
    assert_eq!(
        admitted.load(std::sync::atomic::Ordering::Relaxed),
        before_invalid,
        "invalid query must be refused before Store admission"
    );
    let (status, wrong_revision) = request(
        app.clone(),
        key,
        &format!(
            "?indexGeneration={generation}&indexRevision={}",
            revision + 100
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(wrong_revision["error"]["code"], "revision_conflict");
    let (status, wrong_generation) = request(
        app.clone(),
        key,
        &format!("?indexGeneration=11111111-1111-4111-8111-111111111111&indexRevision={revision}"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "wrong generation must never fall back"
    );
    assert_eq!(wrong_generation["error"]["code"], "revision_conflict");
    let (status, missing) = request(
        app.clone(),
        key,
        &format!("?scope=file&sourceSetId={source}&language=javascript&path=missing.js"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert_eq!(missing["error"]["code"], "not_found");
    let (status, _) = request(
        app.clone(),
        key,
        "?scope=file&sourceSetId=source-set:v1:garbage&language=javascript&path=sample.js",
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let foreign_source = format!("source-set:v1:path-{}", "0".repeat(64));
    let (status, foreign) = request(
        app.clone(),
        key,
        &format!("?scope=file&sourceSetId={foreign_source}&language=javascript&path=sample.js"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{foreign}");
    assert_eq!(foreign["error"]["code"], "not_found");
    let sid = file["payload"]["declarations"][0]["sid"].as_str().unwrap();
    let (status, declaration) = request(
        app.clone(),
        key,
        &format!("?scope=declaration&sourceSetId={source}&path=sample.js&sid={sid}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{declaration}");
    assert_eq!(
        declaration["payload"]["declarations"][0]["metrics"]["parameters"],
        1
    );
    for malformed in [
        "sid:v1:not-hex",
        "sid:v1:ABCDEF00000000000000000000000000",
        "sid:v1:0000000000000000000000000000000",
    ] {
        let (status, value) = request(
            app.clone(),
            key,
            &format!("?scope=declaration&sourceSetId={source}&path=sample.js&sid={malformed}"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{value}");
        assert_eq!(value["error"]["code"], "invalid_request");
    }
    let (status, unknown_sid) = request(app.clone(), key, &format!("?scope=declaration&sourceSetId={source}&path=sample.js&sid=sid:v1:00000000000000000000000000000000")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{unknown_sid}");
    assert_eq!(unknown_sid["error"]["code"], "not_found");
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
    assert_eq!(directory["payload"]["directories"][0]["metrics"]["loc"], 2);
    let (status, nested) = request(
        app.clone(),
        key,
        &format!("?scope=directory&sourceSetId={source}&path=deep"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{nested}");
    assert_eq!(nested["payload"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(nested["payload"]["files"][0]["path"], "deep/extra.js");
    assert_eq!(nested["payload"]["directories"][0]["metrics"]["loc"], 1);
    let (status, empty) = request(
        app.clone(),
        key,
        &format!("?scope=file&sourceSetId={source}&language=javascript&path=empty.js"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(empty["payload"]["files"][0]["declarationCount"], 0);
    let (status, missing_dir) = request(
        app.clone(),
        key,
        &format!("?scope=directory&sourceSetId={source}&path=absent"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing_dir}");
    assert_eq!(
        directory["payload"]["directories"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn selected_request_cancelled_during_read_and_serialization() {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    for phase in ["read", "serialize"] {
        let (_base, runtime, browser, key) = fixture().await;
        assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 0);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let (cancelled_tx, cancelled_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let resume = Mutex::new(resume_rx);
        let progress = AtomicUsize::new(0);
        let once = std::sync::atomic::AtomicBool::new(true);
        browser.set_selected_response_hook_for_tests(Arc::new(move |stage| {
            // Inside the first document's attestation, before any memo entry.
            let at_read =
                stage == "metrics_read_progress" && progress.fetch_add(1, Ordering::Relaxed) == 50;
            let at_serialize = stage == "metrics_serialize";
            if ((phase == "read" && at_read) || (phase == "serialize" && at_serialize))
                && once.swap(false, Ordering::AcqRel)
            {
                entered_tx.send(()).unwrap();
                resume.lock().unwrap().recv().unwrap();
            }
            if stage == "metrics_cancelled" {
                let _ = cancelled_tx.try_send(());
            }
            if stage == "metrics_worker_done" {
                let _ = done_tx.try_send(());
            }
        }));
        let app = browser.router();
        let worker = tokio::spawn({
            let key = key.clone();
            let app = app.clone();
            async move { request(app, &key, "").await }
        });
        tokio::time::timeout(
            trellis::testing::HANG_BACKSTOP,
            tokio::task::spawn_blocking(move || entered_rx.recv().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        if phase == "read" {
            assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 0);
        }
        worker.abort();
        assert!(
            worker.await.unwrap_err().is_cancelled(),
            "no partial HTTP response"
        );
        resume_tx.send(()).unwrap();
        tokio::time::timeout(
            trellis::testing::HANG_BACKSTOP,
            tokio::task::spawn_blocking(move || cancelled_rx.recv().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        tokio::time::timeout(
            trellis::testing::HANG_BACKSTOP,
            tokio::task::spawn_blocking(move || done_rx.recv().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        if phase == "read" {
            assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 0);
        }
        // Serialization may keep only source intermediates completed before it began.
        let (status, body) = request(app, &key, "").await;
        assert_eq!(status, StatusCode::OK, "{phase}: {body}");
        assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 4);
    }
}

#[tokio::test]
async fn selected_metrics_atomic_bounds_and_deadlines() {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    // A small completed response is used to set a cap at its final byte.
    let (_base, _runtime, browser, key) = fixture().await;
    let app = browser.clone().router();
    let raw = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/checkouts/{key}/metrics"))
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let full_len = to_bytes(raw.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap()
        .len();
    browser.set_metrics_controls_for_tests(full_len - 1, None, false);
    let (status, capped) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{capped}");
    assert_eq!(capped["error"]["code"], "response_too_large");
    browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, true);
    let (status, cancelled) = request(app.clone(), &key, "").await;
    assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{cancelled}");
    assert_eq!(cancelled["error"]["code"], "cancelled");
    browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, false);

    // The blocking hook holds the request at each named phase. The test clock
    // advances only after the phase is reached, then releases the worker.
    for (stage, cap) in [
        ("metrics_parser_progress", 64 * 1024 * 1024),
        ("metrics_serialize_last", full_len + 1),
        ("after_read_finish", 64 * 1024 * 1024),
    ] {
        let parser = stage == "metrics_parser_progress";
        let (_base, runtime, browser, key) = if parser {
            fixture_with_startup_hook(None, true, true).await
        } else {
            fixture().await
        };
        let query = if parser {
            format!(
                "?scope=file&sourceSetId=source-set:v1:path-{key}&language=javascript&path=parser.js"
            )
        } else {
            String::new()
        };
        let (entered_tx, mut entered_rx) = tokio::sync::mpsc::channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let resume = Mutex::new(resume_rx);
        let once = AtomicBool::new(true);
        let parser_callbacks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let callbacks_in_hook = parser_callbacks.clone();
        browser.set_selected_response_hook_for_tests(Arc::new(move |called| {
            if called == "metrics_parser_progress" {
                callbacks_in_hook.fetch_add(1, Ordering::Relaxed);
            }
            if called == stage && once.swap(false, Ordering::AcqRel) {
                let _ = entered_tx.try_send(());
                let _ = resume.lock().unwrap().recv();
            }
        }));
        let deadline = Duration::from_millis(1000);
        browser.set_metrics_clock_for_tests(std::time::Instant::now() - deadline);
        browser.set_metrics_controls_for_tests(cap, Some(deadline), false);
        let app = browser.clone().router();
        let request_key = key.clone();
        let request_query = query.clone();
        let mut worker =
            tokio::spawn(async move { request(app, &request_key, &request_query).await });
        let entered = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            tokio::select! {
                entered = entered_rx.recv() => {
                    assert!(entered.is_some(), "metrics phase {stage} hook disconnected");
                }
                result = &mut worker => {
                    panic!("metrics phase {stage} was not reached: HTTP worker finished {result:?}");
                }
            }
        })
        .await;
        if entered.is_err() {
            worker.abort();
        }
        entered.unwrap_or_else(|_| panic!("metrics phase {stage} was not reached before backstop"));
        browser.set_metrics_clock_skew_for_tests(1001);
        resume_tx.send(()).unwrap();
        let (status, value) = worker.await.unwrap();
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{stage}: {value}");
        assert_eq!(value["error"]["code"], "deadline_exceeded", "{stage}");
        if stage == "metrics_parser_progress" {
            assert_eq!(
                parser_callbacks.load(Ordering::Relaxed),
                1,
                "parse must stop in the first callback after deadline expiry"
            );
            assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 0);
            browser.set_metrics_clock_skew_for_tests(0);
            browser.set_metrics_controls_for_tests(64 * 1024 * 1024, None, false);
            let (status, body) = request(browser.clone().router(), &key, &query).await;
            assert_eq!(status, StatusCode::OK, "following parser request: {body}");
            assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 1);
        }
    }
}

#[tokio::test]
async fn selected_metrics_reports_catching_up_from_same_admission() {
    use std::sync::{Mutex, mpsc};
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let resume = Mutex::new(resume_rx);
    let hook = Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume.lock().unwrap().recv().unwrap();
    });
    let (_base, runtime, browser, key) = fixture_with_startup_hook(Some(hook), false, false).await;
    tokio::time::timeout(
        trellis::testing::HANG_BACKSTOP,
        tokio::task::spawn_blocking(move || entered_rx.recv().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let app = browser.router();
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/checkouts/{key}/metrics"))
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["X-Trellis-Catching-Up"], "true");
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 64 * 1024 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["catchingUp"], true);
    assert_eq!(body["payload"]["files"].as_array().unwrap().len(), 4);
    resume_tx.send(()).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while runtime.catching_up() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
}
