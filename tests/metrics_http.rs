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
    assert_eq!(response["payload"]["files"].as_array().unwrap().len(), 3);
    assert_eq!(response["payload"]["declarations"], serde_json::json!([]));
    assert_eq!(
        response["payload"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["deep/extra.js", "empty.js", "sample.js"]
    );
    assert_eq!(
        response["payload"]["directories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["", "deep"]
    );
    assert_eq!(response["payload"]["directories"][0]["declarationCount"], 2);
    assert_eq!(
        response["payload"]["directories"][0]["metrics"]["cyclomatic"],
        2
    );
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
        10.0
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
    let (wrong_root, _) = request(app.clone(), &"0".repeat(64), "").await;
    assert_ne!(wrong_root, StatusCode::OK);
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
        "?wat=1",
        "?scope=revision&scope=file",
        "?indexGeneration=bad",
        "?scope=file&sourceSetId=x&language=javascript&path=../a",
        "?scope=declaration&sourceSetId=source-set:v1:garbage&path=sample.js&sid=sid:v1:not-hex",
    ] {
        let (status, _) = request(app.clone(), key, query).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
    }
    assert_eq!(
        admitted.load(std::sync::atomic::Ordering::Relaxed),
        before_invalid,
        "invalid query must be refused before Store admission"
    );
    let (status, _) = request(
        app.clone(),
        key,
        &format!(
            "?indexGeneration={generation}&indexRevision={}",
            revision + 100
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
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
        2
    );
}

#[tokio::test]
async fn selected_request_cancelled_on_drop_and_not_published() {
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    let (_base, runtime, browser, key) = fixture().await;
    assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 0);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let (cancelled_tx, cancelled_rx) = mpsc::sync_channel(1);
    let resume = Mutex::new(resume_rx);
    let once = AtomicBool::new(true);
    browser.set_selected_response_hook_for_tests(Arc::new(move |stage| {
        if stage == "before_work" && once.swap(false, Ordering::AcqRel) {
            entered_tx.send(()).unwrap();
            resume.lock().unwrap().recv().unwrap();
        }
        if stage == "metrics_cancelled" {
            let _ = cancelled_tx.try_send(());
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
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    resume_tx.send(()).unwrap();
    tokio::time::timeout(
        trellis::testing::HANG_BACKSTOP,
        tokio::task::spawn_blocking(move || cancelled_rx.recv().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 0);
    let (status, body) = request(app, &key, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(runtime.metrics_memo_entries_for_tests().unwrap(), 3);
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
    // expires while the HTTP caller still waits, then releases the worker.
    for (stage, cap) in [
        ("before_work", 64 * 1024 * 1024),
        ("metrics_serialize_last", full_len + 1),
        ("after_read_finish", 64 * 1024 * 1024),
    ] {
        let (_base, _runtime, browser, key) = fixture().await;
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (resume_tx, resume_rx) = mpsc::sync_channel(1);
        let resume = Mutex::new(resume_rx);
        let once = AtomicBool::new(true);
        browser.set_selected_response_hook_for_tests(Arc::new(move |called| {
            if called == stage && once.swap(false, Ordering::AcqRel) {
                entered_tx.send(()).unwrap();
                resume.lock().unwrap().recv().unwrap();
            }
        }));
        browser.set_metrics_controls_for_tests(cap, Some(Duration::from_millis(1000)), false);
        let app = browser.clone().router();
        let worker = tokio::spawn(async move { request(app, &key, "").await });
        tokio::time::timeout(
            trellis::testing::HANG_BACKSTOP,
            tokio::task::spawn_blocking(move || entered_rx.recv().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        browser.set_metrics_clock_skew_for_tests(1001);
        resume_tx.send(()).unwrap();
        let (status, value) = worker.await.unwrap();
        assert_eq!(status, StatusCode::REQUEST_TIMEOUT, "{stage}: {value}");
        assert_eq!(value["error"]["code"], "deadline_exceeded", "{stage}");
    }
}
