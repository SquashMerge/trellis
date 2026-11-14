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

/// The last stage a watched test body entered, and when (#1078).
#[derive(Clone)]
struct Stage(Arc<std::sync::Mutex<(String, std::time::Instant)>>);
impl Stage {
    fn enter(&self, what: String) {
        *self.0.lock().unwrap() = (what, std::time::Instant::now());
    }
    fn last(&self) -> (String, std::time::Instant) {
        self.0.lock().unwrap().clone()
    }
}

/// Run `body` as `#[tokio::test]` does, on a current-thread runtime, but on its own thread
/// while this one watches it. A call that blocks the runtime's thread also stops every Tokio
/// timer on it, so no timeout inside `body` can report it: the watchdog does, and names the
/// stage `body` last entered. Each stage has the hang backstop, which is never a speed limit.
fn run_watched<F>(body: impl FnOnce(Stage) -> F + Send + 'static)
where
    F: std::future::Future<Output = ()>,
{
    let stage = Stage(Arc::new(std::sync::Mutex::new((
        "starting".into(),
        std::time::Instant::now(),
    ))));
    let entered = stage.clone();
    let (done, finished) = std::sync::mpsc::channel::<()>();
    let test = std::thread::Builder::new()
        .name("watched-test-body".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(body(entered.clone()));
            // Dropping the runtime waits for its blocking tasks, with no bound.
            entered.enter("dropping the Tokio runtime".into());
            drop(runtime);
            drop(done);
        })
        .unwrap();
    loop {
        let (what, since) = stage.last();
        let left = (since + trellis::testing::HANG_BACKSTOP)
            .saturating_duration_since(std::time::Instant::now());
        match finished.recv_timeout(left) {
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => assert!(
                stage.last().1 != since,
                "the test body did not get past this stage: {what} (hang)"
            ),
            // Finished or panicked: the join below reports which.
            _ => break,
        }
    }
    if let Err(panic) = test.join() {
        std::panic::resume_unwind(panic);
    }
}

/// One authenticated call under the hang backstop. `what` names it if it never answers.
async fn call_named(
    app: &Router,
    method: &str,
    path: &str,
    body: Value,
    what: &str,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    tokio::time::timeout(
        trellis::testing::HANG_BACKSTOP,
        call(app, method, path, body, true),
    )
    .await
    .unwrap_or_else(|_| panic!("{what} did not answer (hang)"))
}

#[test]
fn production_new_evidence_routes_keep_before_finish_and_final_selected_root_fences() {
    run_watched(root_fence_iterations);
}

/// Each iteration builds its own runtime and loses its root mid-read. The stage names say
/// which iteration and which call a hang was in (#1078).
async fn root_fence_iterations(progress: Stage) {
    let cases = [
        ("before_read_finish", "symbols"),
        ("after_handler", "symbols"),
        ("before_read_finish", "syntax-matches"),
        ("after_handler", "syntax-matches"),
    ];
    for (index, (stage, suffix)) in cases.into_iter().enumerate() {
        let at = format!(
            "iteration {} of {} ({stage}/{suffix})",
            index + 1,
            cases.len()
        );
        root_fence_iteration(&progress, &at, stage, suffix).await;
    }
}

async fn root_fence_iteration(progress: &Stage, at: &str, stage: &'static str, suffix: &str) {
    progress.enter(format!("{at}: building the checkout and its registry"));
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
        ProvisionedBrowser::new(registry, TOKEN.into(), "127.0.0.1:7331".parse().unwrap()).unwrap();
    let app = browser.clone().router();
    let prefix = format!("/api/checkouts/{}", identity.root_key);
    progress.enter(format!("{at}: status poll (activation and first head)"));
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
    .unwrap_or_else(|_| panic!("{at}: status never answered 200 (hang)"));
    let pin: IndexPin = serde_json::from_value(status["revision"].clone()).unwrap();
    progress.enter(format!("{at}: outline read before the root moves"));
    let (_, _, outline) = call_named(
        &app,
        "GET",
        &outline_path(&prefix, pin),
        Value::Null,
        &format!("{at}: the outline read before the root moves"),
    )
    .await;
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
    progress.enter(format!("{at}: fenced {method} {suffix} whose root moves"));
    let (status, _, error) = call_named(
        &app,
        method,
        &path,
        body,
        &format!("{at}: the fenced {method} {suffix} whose root moves"),
    )
    .await;
    assert!(changed.load(Ordering::Acquire), "{stage}/{suffix}");
    assert_eq!(status, 409, "{stage}/{suffix}: {error}");
    assert_eq!(error["error"]["code"], "root_changed");
    assert!(error.get("items").is_none());
    // The iteration's values drop here, in reverse order of declaration.
    progress.enter(format!(
        "{at}: dropping the browser, registry and temporary directories"
    ));
}
