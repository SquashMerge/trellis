#![cfg(feature = "test-support")]
mod common;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use tower::ServiceExt;
use trellis::{auth, http, indexer::IndexOptions, model::*, store::Store};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn setup() -> (tempfile::TempDir, Store, Arc<http::DaemonState>, Router) {
    setup_with_tick(true)
}

// Forced maintenance ticks need sole control of the serving session and native stream.
// The background queue tick can otherwise defer a forced tick without an error (#131).
fn setup_without_tick() -> (tempfile::TempDir, Store, Arc<http::DaemonState>, Router) {
    setup_with_tick(false)
}

fn setup_with_tick(start_tick: bool) -> (tempfile::TempDir, Store, Arc<http::DaemonState>, Router) {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
    // Daemon admission publishes the virgin queue before retaining writer SH
    // or starting startup reconciliation. Later HTTP requests may then commit under that SH.
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    let state = http::new(
        store.clone(),
        IndexOptions::new(workspace),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    if start_tick {
        // A running worker accepts only a session whose startup
        // reconciliation committed a validated head.
        let session = trellis::index_coordinator::establish_serving_session(
            &store,
            None,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        state.retain_serving_session(session);
    } else {
        let session = store.writer_session_for_tests().unwrap();
        state.retain_serving_session_without_tick_for_tests(session);
    }
    let router = http::router(state.clone());
    (dir, store, state, router)
}
async fn call(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let code = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
#[tokio::test]
async fn guards_public_and_private() {
    // No startup reconciliation: /api/status has no head and answers 503.
    let (_d, _store, _state, app) = setup_without_tick();
    for (path, host, origin, token, want) in [
        ("/healthz", "127.0.0.1:7331", None, None, 200),
        ("/", "localhost:7331", None, None, 200),
        ("/api/status", "127.0.0.1:7331", None, None, 401),
        ("/api/status", "127.0.0.1:7331", None, Some(TOKEN), 503),
        ("/healthz", "evil.example", None, None, 403),
        (
            "/",
            "127.0.0.1:7331",
            Some("http://evil.example"),
            None,
            403,
        ),
        (
            "/api/status",
            "127.0.0.1:7331",
            Some("null"),
            Some(TOKEN),
            403,
        ),
        (
            "/api/status",
            "localhost:7331",
            Some("http://localhost:7331"),
            Some(TOKEN),
            503,
        ),
    ] {
        let mut req = Request::builder().uri(path).header("host", host);
        if let Some(o) = origin {
            req = req.header("origin", o)
        }
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"))
        }
        let response = app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), want, "{path} {host}");
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(
            response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("frame-ancestors 'none'")
        );
        assert!(
            !response
                .headers()
                .contains_key("access-control-allow-origin")
        );
    }
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/api/index")
        .header("host", "127.0.0.1:7331")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), 403);
}
#[tokio::test]
async fn validation_and_limit() {
    let (_d, _store, _state, app) = setup();
    for path in [
        "/api/source?path=../secret",
        "/api/source?path=%2Fetc%2Fpasswd",
        "/api/source?path=C:%5Csecret",
        "/api/source?path=a%2F..%2Fb",
    ] {
        assert_eq!(call(&app, "GET", path, Value::Null).await.0, 400)
    }
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/sequence",
            json!({"seed":"x","expectedRevision":6})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(
            &app,
            "POST",
            "/api/navigation",
            json!({"path":"../secret","line":1})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        call(&app, "POST", "/api/index", json!({"workspaceRoot":"/tmp"}))
            .await
            .0,
        400
    );
    assert_eq!(
        call(&app, "POST", "/api/index", json!({"expectedRevision":9}))
            .await
            .0,
        400
    );
    let stale = IndexPin {
        index_generation: _store.index_baseline().unwrap().index_generation,
        index_revision: 9,
    };
    let (code, accepted) = call(
        &app,
        "POST",
        "/api/index",
        json!({"expectedRevision":stale}),
    )
    .await;
    assert_eq!(code, 202, "{accepted}");
    assert_eq!(accepted["state"], "queued");
    let stale_id = accepted["id"].as_str().unwrap();
    let failed = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (_, row) = call(&app, "GET", &format!("/api/jobs/{stale_id}"), Value::Null).await;
            if !row["finishedAt"].is_null() {
                break row;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"]["code"], "revision_conflict");
    let req = Request::builder()
        .method("POST")
        .uri("/api/index")
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from(vec![b' '; 1024 * 1024 + 1]))
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), 413);
}
#[tokio::test]
async fn source_is_snapshot_and_revision_checked() {
    let (_d, store, state, app) = setup_without_tick();
    let workspace = _d.path().join("workspace");
    std::fs::write(workspace.join("a.js"), "cached secret-free source").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &IndexOptions::new(workspace),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(graph.files[0].text, "cached secret-free source");
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            state
                .retained_serving_session()
                .unwrap()
                .writer_guard()
                .unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let pin = store.status().unwrap().revision;
    let (code, body) = call(
        &app,
        "GET",
        &format!(
            "/api/source?path=a.js&indexGeneration={}&indexRevision={}",
            pin.index_generation, pin.index_revision
        ),
        Value::Null,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(body["revision"], json!(pin));
    assert_eq!(body["file"]["text"], "cached secret-free source");
    assert_eq!(
        call(
            &app,
            "GET",
            &format!(
                "/api/source?path=a.js&indexGeneration={}&indexRevision=0",
                pin.index_generation
            ),
            Value::Null
        )
        .await
        .0,
        409
    );
    assert_eq!(
        call(&app, "GET", "/api/source?path=absent.js", Value::Null)
            .await
            .0,
        404
    );
}
#[tokio::test]
async fn dependency_status_whole_response_fence() {
    let (dir, store, state, app) = setup_without_tick();
    std::fs::write(dir.path().join("workspace/a.js"), "function go() {}\n").unwrap();
    let options = IndexOptions::new(dir.path().join("workspace"));
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
            .unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            state
                .retained_serving_session()
                .unwrap()
                .writer_guard()
                .unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let (code, result) = call(&app, "GET", "/api/dependencies", Value::Null).await;
    assert_eq!(code, StatusCode::OK, "{result}");
    assert_eq!(result["workspaceRevision"], json!(pin));
    assert_eq!(result["catalogId"], Value::Null);
    assert_eq!(result["packages"], json!([]));
    assert_eq!(result["symbolCount"], 0);
}
#[tokio::test]
async fn jobs_publish_and_cancel() {
    let (_d, store, state, app) = setup();
    let (code, job) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(code, 202);
    let id = job["id"].as_str().unwrap();
    let completed = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (_, j) = call(&app, "GET", &format!("/api/jobs/{id}"), Value::Null).await;
            if !j["finishedAt"].is_null() {
                break j;
            }
            tokio::task::yield_now().await
        }
    })
    .await
    .unwrap();
    assert_eq!(completed["state"], "done");
    let head = store.status().unwrap().revision;
    assert_eq!(
        (head.index_revision, completed["revision"].clone()),
        (1, json!(head)),
        "mandatory startup reconciliation publishes r1; the unchanged accepted Q completes DONE at r1"
    );
    let (_, cancelled) = call(&app, "POST", &format!("/api/jobs/{id}/cancel"), Value::Null).await;
    assert_eq!(cancelled["state"], "done");
    assert_eq!(
        store.status().unwrap().revision,
        head,
        "cancelling a finished job publishes nothing"
    );
    state.cancel_active();
}
#[tokio::test]
async fn queued_request_is_durable_and_non_cancellable_during_lock_contention() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("one.js"), "function one() {}\n").unwrap();
    let store = Store::open_for_tests(&dir.path().join("state"), &root).unwrap();
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    let held = store.writer_session_for_tests().unwrap();
    let state = http::new(
        store.clone(),
        IndexOptions::new(root),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let app = http::router(state.clone());
    let (code, accepted) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["state"], "queued");
    assert!(accepted["submittedAt"].is_string());
    assert!(accepted["startedAt"].is_null());
    assert!(accepted["finishedAt"].is_null());
    let id = accepted["id"].as_str().unwrap();
    let (code, current) = call(&app, "GET", "/api/jobs/current", Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(current["id"], id);
    let (code, rejected) = call(&app, "POST", &format!("/api/jobs/{id}/cancel"), Value::Null).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(rejected["error"]["code"], "request_not_cancellable");
    assert_eq!(store.request_by_id(id).unwrap().unwrap().state, "queued");
    drop(held);
    let done = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (code, row) = call(&app, "GET", &format!("/api/jobs/{id}"), Value::Null).await;
            assert_eq!(code, StatusCode::OK, "{row}");
            if !row["finishedAt"].is_null() {
                break row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(done["state"], "done", "{done}");
    assert!(done["startedAt"].is_string());
    assert_eq!(
        store.status().unwrap().revision,
        serde_json::from_value(done["revision"].clone()).unwrap()
    );
    let (code, again) = call(&app, "POST", &format!("/api/jobs/{id}/cancel"), Value::Null).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(again, done);
    drop(state);
    let reopened = Store::open_for_tests(
        &dir.path().join("state"),
        std::path::Path::new(store.workspace_root()),
    )
    .unwrap();
    assert_eq!(reopened.request_by_id(id).unwrap().unwrap().state, "done");
}

#[test]
fn token_security() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let d = tempfile::tempdir().unwrap();
    let path = d.path().join("token");
    let token = auth::load_or_create_token(&path).unwrap();
    assert!(auth::valid_token(&token));
    assert_eq!(auth::load_or_create_token(&path).unwrap(), token);
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(auth::load_or_create_token(&path).is_err());
    let link = d.path().join("link");
    symlink(&path, &link).unwrap();
    assert!(auth::load_or_create_token(&link).is_err());
}
#[test]
fn rejects_bad_startup() {
    let (_d, store, _state, _app) = setup();
    assert!(
        http::new(
            store.clone(),
            IndexOptions::new(".".into()),
            TOKEN.into(),
            "0.0.0.0:7331".parse().unwrap()
        )
        .is_err()
    );
    assert!(
        http::new(
            store,
            IndexOptions::new(".".into()),
            "bad".into(),
            "127.0.0.1:7331".parse().unwrap()
        )
        .is_err()
    );
}

#[tokio::test]
async fn active_job_cancellation_does_not_publish() {
    let (d, store, state, app) = setup();
    let source = (0..15000)
        .map(|i| format!("function f{i}() {{ console.log({i}); }}\n"))
        .collect::<String>();
    std::fs::write(d.path().join("workspace/large.js"), source).unwrap();
    // The running worker's watcher may publish the new file first; count
    // revisions from its settled head.
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while state.checkout_watch_pending() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("watcher did not settle");
    let settled = store.index_baseline().unwrap().index_revision;
    // A real edit after the settled head: exactly one publication covers it,
    // by the accepted Q or by the watcher with Q then unchanged.
    let mut edited = std::fs::read_to_string(d.path().join("workspace/large.js")).unwrap();
    edited.push_str("function edited() { console.log(-1); }\n");
    std::fs::write(d.path().join("workspace/large.js"), edited).unwrap();
    let (code, j) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(code, 202);
    let id = j["id"].as_str().unwrap();
    let (code, _) = call(&app, "POST", &format!("/api/jobs/{id}/cancel"), Value::Null).await;
    assert_eq!(code, 409);
    // The 15k-function index runs beside other verifier lanes in CI. This is a
    // bounded completion guard, not an index-latency requirement.
    let terminal = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (_, j) = call(&app, "GET", &format!("/api/jobs/{id}"), Value::Null).await;
            if !j["finishedAt"].is_null() {
                break j;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(terminal["state"], "done");
    let head = store.index_baseline().unwrap();
    assert_eq!(terminal["revision"], json!(head));
    assert_eq!(
        head.index_revision,
        settled + 1,
        "the edit publishes exactly once; cancellation does not add one"
    );
}

#[tokio::test]
async fn local_design_assets_preserve_same_origin_guards() {
    let (_d, _store, _state, app) = setup();
    for (path, mime) in [
        ("/shell.js", "text/javascript; charset=utf-8"),
        ("/fonts/jetbrains-mono-latin.woff2", "font/woff2"),
        ("/fonts/inter-variable.woff2", "font/woff2"),
        ("/fonts/manrope-variable.woff2", "font/woff2"),
        ("/fonts/JetBrainsMono-OFL.txt", "text/plain; charset=utf-8"),
        ("/fonts/Inter-OFL.txt", "text/plain; charset=utf-8"),
        ("/fonts/Manrope-OFL.txt", "text/plain; charset=utf-8"),
        ("/brand/trellis-mark-transparent-v1.png", "image/png"),
        ("/brand/trellis-logo-dark-v1.webp", "image/webp"),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "127.0.0.1:7331")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-type"], mime);
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        let csp = response.headers()["content-security-policy"]
            .to_str()
            .unwrap();
        assert!(csp.contains("font-src 'self'"));
        assert!(csp.contains("img-src 'self' data:"));
        assert!(!csp.contains("unsafe-inline"));
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        if mime == "font/woff2" {
            assert_eq!(&bytes[..4], b"wOF2");
        } else if mime == "image/png" {
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        } else if mime == "image/webp" {
            assert_eq!(&bytes[..4], b"RIFF");
            assert_eq!(&bytes[8..12], b"WEBP");
        } else if path.ends_with("OFL.txt") {
            assert!(String::from_utf8_lossy(&bytes).contains("SIL OPEN FONT LICENSE"));
        }
        let bad_host = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_host.status(), 403);
        let bad = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("host", "127.0.0.1:7331")
                    .header("origin", "https://evil.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad.status(), 403);
    }
    for path in [
        "/fonts/missing.woff2",
        "/fonts/inter-latin.woff2",
        "/fonts/manrope-latin.woff2",
        "/fonts/space-grotesk-latin.woff2",
        "/fonts/SpaceGrotesk-OFL.txt",
        "/brand/missing.png",
    ] {
        assert_eq!(call(&app, "GET", path, Value::Null).await.0, 404);
    }
}

#[tokio::test]
async fn live_control_corruption_returns_typed_503_and_hard_latches_clones() {
    for case in ["stats-source", "real-symbol", "input-classes"] {
        // Without the background queue tick: the test checks the corruption
        // latch as reads observe it, and the tick's own rebuild of the
        // disposable index would otherwise clear the latch mid-test (#131).
        let (dir, store, state, app) = setup_without_tick();
        let workspace = dir.path().join("workspace");
        std::fs::write(workspace.join("a.js"), "function go() { measured(); }\n").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &IndexOptions::new(workspace),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                state
                    .retained_serving_session()
                    .unwrap()
                    .writer_guard()
                    .unwrap(),
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        let index = std::fs::read_dir(dir.path().join("state/cache/indexes"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap()
            .join("index.db");
        let db = rusqlite::Connection::open(index).unwrap();
        let route = match case {
            "stats-source" => {
                db.execute("UPDATE index_metadata SET stats='not-json'", [])
                    .unwrap();
                format!(
                    "/api/source?path=a.js&indexGeneration={}&indexRevision={}",
                    pin.index_generation, pin.index_revision
                )
            }
            "real-symbol" => {
                db.execute(
                    "UPDATE index_metadata SET index_revision=CAST(1.5 AS REAL)",
                    [],
                )
                .unwrap();
                format!(
                    "/api/files?indexGeneration={}&indexRevision={}",
                    pin.index_generation, pin.index_revision
                )
            }
            "input-classes" => {
                db.execute(
                    "UPDATE revision_capture_inputs SET payload='not-json' WHERE input_key='root:.'",
                    [],
                )
                .unwrap();
                format!(
                    "/api/classes?indexGeneration={}&indexRevision={}",
                    pin.index_generation, pin.index_revision
                )
            }
            _ => unreachable!(),
        };
        drop(db);

        let (status, body) = call(&app, "GET", &route, Value::Null).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{case}: {body}");
        assert_eq!(
            body["error"]["code"], "incompatible_index",
            "{case}: {body}"
        );
        let (status, body) = call(&app, "GET", "/api/status", Value::Null).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{case}: {body}");
        assert_eq!(
            body["error"]["code"], "incompatible_index",
            "{case}: {body}"
        );
    }
}

fn corrupt_recovery_fixture(
    retain_old: bool,
    alias_options: bool,
) -> (
    tempfile::TempDir,
    Store,
    Arc<http::DaemonState>,
    Router,
    std::path::PathBuf,
    trellis::store::topology::TopologyRoots,
    trellis::store::topology::WorkspaceIdentity,
    IndexPin,
) {
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let roots = TopologyRoots::isolated_for_tests(
        dir.path().join("state/cache"),
        dir.path().join("state/data"),
    );
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let index = roots.index_db(&identity);
    let initial = Store::open_for_tests(&dir.path().join("state"), &root).unwrap();
    let mut options = IndexOptions::new(root.clone());
    options.max_file_bytes = 2_097_152;
    let cancel = Arc::new(AtomicBool::new(false));
    let (pin, old_owner) =
        trellis::index_coordinator::reconcile_workspace(&initial, &options, &cancel, |_| {})
            .unwrap();
    std::fs::write(&index, b"bad sqlite index header").unwrap();
    let pending = Store::open_for_tests(&dir.path().join("state"), &root).unwrap();
    if alias_options {
        // The final workspace component is a real directory; only its parent
        // spelling differs. The queue pins the same captured root inode.
        let alias = dir.path().join("root-alias");
        std::os::unix::fs::symlink(dir.path(), &alias).unwrap();
        options.workspace_root = alias.join("workspace");
        assert_ne!(
            options.workspace_root,
            std::fs::canonicalize(&root).unwrap()
        );
        assert_eq!(
            std::fs::canonicalize(&options.workspace_root).unwrap(),
            std::fs::canonicalize(&root).unwrap()
        );
    }
    let state = http::new(
        pending.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    if retain_old {
        state.retain_serving_session(old_owner);
    }
    let app = http::router(state.clone());
    (dir, pending, state, app, index, roots, identity, pin)
}

async fn finished_index_job(app: &Router, id: &str) -> Value {
    // Keep a finite CI progress guard without hot-polling the queue worker.
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (_, job) = call(app, "GET", &format!("/api/jobs/{id}"), Value::Null).await;
            if !job["finishedAt"].is_null() {
                break job;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("index job did not finish")
}

#[tokio::test]
async fn explicit_recovery_without_startup_owner_rejects_pin_then_installs_same_new_leader() {
    let (_dir, store, state, app, index, _roots, identity, old) =
        corrupt_recovery_fixture(false, true);
    assert!(state.retained_serving_session().is_err());
    let bytes = std::fs::read(&index).unwrap();
    let (conflict, rejected) =
        call(&app, "POST", "/api/index", json!({"expectedRevision":old})).await;
    assert_eq!(conflict, StatusCode::CONFLICT);
    assert_eq!(rejected["error"]["code"], "revision_conflict");
    assert_eq!(std::fs::read(&index).unwrap(), bytes);
    assert_eq!(
        call(&app, "GET", "/api/jobs/current", Value::Null).await.1,
        Value::Null
    );
    let (status, admitted) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{admitted}");
    let done = finished_index_job(&app, admitted["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done", "{done}");
    // Recreation publishes r1; the claimed request's capture is unchanged
    // against it, so it completes at r1 without another revision.
    assert_eq!(done["revision"]["indexRevision"], 1);
    assert_ne!(
        done["revision"]["indexGeneration"],
        json!(old)["indexGeneration"]
    );
    assert!(state.retained_serving_session().unwrap().verify().is_ok());
    assert_eq!(
        store.status().unwrap().revision,
        serde_json::from_value(done["revision"].clone()).unwrap()
    );
    state
        .retained_serving_session()
        .unwrap()
        .belongs_to(&identity)
        .unwrap();
    let (ready, status) = call(&app, "GET", "/api/status", Value::Null).await;
    assert_eq!(ready, StatusCode::OK);
    assert_eq!(status["revision"], done["revision"]);
    let (stale_code, stale_body) = call(
        &app,
        "GET",
        &format!(
            "/api/source?path=a.js&indexGeneration={}&indexRevision={}",
            old.index_generation, old.index_revision
        ),
        Value::Null,
    )
    .await;
    assert_eq!(stale_code, StatusCode::CONFLICT, "{stale_body}");
    assert_eq!(stale_body["error"]["code"], "revision_conflict");
    let (accepted, queued) =
        call(&app, "POST", "/api/index", json!({"expectedRevision":old})).await;
    assert_eq!(accepted, StatusCode::ACCEPTED, "{queued}");
    assert_eq!(queued["state"], "queued");
    let failed = finished_index_job(&app, queued["id"].as_str().unwrap()).await;
    assert_eq!(failed["state"], "failed", "{failed}");
    assert_eq!(failed["error"]["code"], "revision_conflict");
    assert_eq!(
        store.status().unwrap().revision,
        serde_json::from_value(done["revision"].clone()).unwrap()
    );
}

#[tokio::test]
async fn explicit_recovery_releases_only_daemon_old_owner_and_retries_after_foreign_reader() {
    let (_dir, store, state, app, index, roots, identity, old) =
        corrupt_recovery_fixture(true, false);
    let bytes = std::fs::read(&index).unwrap();
    let (conflict, rejected) =
        call(&app, "POST", "/api/index", json!({"expectedRevision":old})).await;
    assert_eq!(conflict, StatusCode::CONFLICT);
    assert_eq!(rejected["error"]["code"], "revision_conflict");
    // The worker releases a damaged index's old owner on its own; a rejected
    // request changes nothing.
    assert_eq!(std::fs::read(&index).unwrap(), bytes);
    let reader = roots.index_use_existing(&identity).unwrap();
    let (status, admitted) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{admitted}");
    let id = admitted["id"].as_str().unwrap();
    let queue = store.request_db_path();
    let accepted_bytes = std::fs::read(&queue).unwrap();
    // Poll at the queue tick cadence; CI may schedule its blocking worker late.
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while state.retained_serving_session().is_ok() {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("recovery tick did not release daemon old owner");
    let (_, blocked) = call(&app, "GET", &format!("/api/jobs/{id}"), Value::Null).await;
    assert_eq!(blocked["id"], id);
    assert_eq!(blocked["state"], "queued", "{blocked}");
    assert!(blocked["finishedAt"].is_null());
    assert_eq!(std::fs::read(&index).unwrap(), bytes);
    assert_eq!(std::fs::read(&queue).unwrap(), accepted_bytes);
    assert!(store.status().is_err());
    drop(reader);
    let done = finished_index_job(&app, id).await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(
        done["revision"]["indexRevision"], 1,
        "recreation publishes once; the unchanged claimed request adds no revision"
    );
    assert_ne!(
        done["revision"]["indexGeneration"],
        json!(old)["indexGeneration"]
    );
    assert!(state.retained_serving_session().unwrap().verify().is_ok());
    state
        .retained_serving_session()
        .unwrap()
        .belongs_to(&identity)
        .unwrap();
}

#[tokio::test]
async fn index_request_without_startup_session_takes_over_after_ack() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let store = crate::common::open_store(&dir.path().join("state"), &workspace).unwrap();
    let state = http::new(
        store.clone(),
        IndexOptions::new(workspace),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    let app = http::router(state.clone());
    let (status, accepted) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{accepted}");
    assert_eq!(accepted["state"], "queued");
    assert!(accepted["startedAt"].is_null());
    let done = finished_index_job(&app, accepted["id"].as_str().unwrap()).await;
    assert_eq!(done["state"], "done", "{done}");
    assert_eq!(
        store.status().unwrap().revision,
        serde_json::from_value(done["revision"].clone()).unwrap()
    );
    assert!(state.retained_serving_session().unwrap().verify().is_ok());
}

#[tokio::test]
async fn released_matching_pin_is_typed_http_conflict_without_head_fallback() {
    let (dir, store, state, app) = setup_without_tick();
    let workspace = dir.path().join("workspace");
    store.set_retention_clock_for_tests(1_000, 0);
    std::fs::write(workspace.join("a.js"), "function before() {}\n").unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let session = state.retained_serving_session().unwrap();
    let publish = |expected| {
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &IndexOptions::new(workspace.clone()),
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
                expected,
                &cancel,
            )
            .unwrap()
    };
    let old = publish(store.index_baseline().unwrap());
    std::fs::write(workspace.join("a.js"), "function after() {}\n").unwrap();
    let head = publish(old);
    store.set_retention_clock_for_tests(1_900, 900);
    store
        .release_revision(old, session.writer_guard().unwrap())
        .unwrap();
    let path = |pin: IndexPin| {
        format!(
            "/api/source?path=a.js&indexGeneration={}&indexRevision={}",
            pin.index_generation, pin.index_revision
        )
    };
    let (status, body) = call(&app, "GET", &path(old), Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "pin_expired");
    let (status, body) = call(&app, "GET", &path(head), Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["file"]["text"], "function after() {}\n");
    let foreign = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: old.index_revision,
    };
    let (status, body) = call(&app, "GET", &path(foreign), Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "revision_conflict");
}

#[tokio::test]
async fn verified_leader_expires_due_pin_on_idle_tick_without_publication() {
    let (dir, store, state, app) = setup_without_tick();
    let workspace = dir.path().join("workspace");
    std::fs::write(workspace.join("a.js"), "function idle() {}\n").unwrap();
    store.set_retention_clock_for_tests(1_000, 0);
    let cancel = Arc::new(AtomicBool::new(false));
    let session = state.retained_serving_session().unwrap();
    let publish = |expected| {
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &IndexOptions::new(workspace.clone()),
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
                expected,
                &cancel,
            )
            .unwrap()
    };
    let old = publish(store.index_baseline().unwrap());
    let head = publish(old);
    let pinned = format!(
        "/api/source?path=a.js&indexGeneration={}&indexRevision={}",
        old.index_generation, old.index_revision
    );
    store.set_retention_clock_for_tests(1_899, 899);
    state.force_retention_idle_tick_for_tests().unwrap();
    assert_eq!(
        call(&app, "GET", &pinned, Value::Null).await.0,
        StatusCode::OK
    );
    store.set_retention_clock_for_tests(1_900, 900);
    state.force_retention_idle_tick_for_tests().unwrap();
    let (status, body) = call(&app, "GET", &pinned, Value::Null).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "pin_expired");
    assert_eq!(
        store.status().unwrap().revision,
        head,
        "idle maintenance must not publish"
    );
    assert!(!store.graph_at(Some(head)).unwrap().files.is_empty());
}

#[tokio::test]
async fn expired_pin_reads_require_an_explicit_head_request() {
    // Control retention with the explicit clock and forced ticks below.
    let (dir, store, state, app) = setup_without_tick();
    let workspace = dir.path().join("workspace");
    std::fs::write(workspace.join("a.js"), "function seed() {}\n").unwrap();
    store.set_retention_clock_for_tests(1_000, 0);
    let cancel = Arc::new(AtomicBool::new(false));
    let session = state.retained_serving_session().unwrap();
    let publish = |expected| {
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &IndexOptions::new(workspace.clone()),
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
                expected,
                &cancel,
            )
            .unwrap()
    };
    let old = publish(store.index_baseline().unwrap());
    let seed = store
        .graph_at(Some(old))
        .unwrap()
        .nodes
        .into_iter()
        .find(|node| node.name == "seed")
        .unwrap()
        .id;
    let pin_route = |route: &str, pin: IndexPin| {
        format!(
            "{route}{}indexGeneration={}&indexRevision={}",
            if route.contains('?') { "&" } else { "?" },
            pin.index_generation,
            pin.index_revision
        )
    };
    let head = publish(old); // identical source and measured IDs, but a different revision.
    assert_eq!(store.status().unwrap().revision, head);
    store.set_retention_clock_for_tests(1_900, 900);
    state.force_retention_idle_tick_for_tests().unwrap();
    for (method, route, payload) in [
        ("GET", "/api/source?path=a.js", Value::Null),
        ("GET", "/api/files", Value::Null),
        ("GET", "/api/methods?path=a.js", Value::Null),
        ("GET", "/api/classes", Value::Null),
    ] {
        let (status, body) = call(&app, method, &pin_route(route, old), payload).await;
        assert_eq!(status, StatusCode::CONFLICT, "{method} {route}: {body}");
        assert_eq!(
            body["error"]["code"], "pin_expired",
            "{method} {route}: {body}"
        );
    }
    let (status, body) = call(
        &app,
        "POST",
        "/api/sequence",
        json!({"seed":seed,"expectedRevision":old}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "pin_expired");
    let foreign = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        ..old
    };
    let missing = IndexPin {
        index_revision: head.index_revision + 1,
        ..old
    };
    for invalid in [foreign, missing] {
        for route in ["/api/files", "/api/source?path=a.js"] {
            let (status, body) = call(&app, "GET", &pin_route(route, invalid), Value::Null).await;
            assert_eq!(status, StatusCode::CONFLICT, "{route}: {body}");
            assert_eq!(
                body["error"]["code"], "revision_conflict",
                "{route}: {body}"
            );
        }
    }
    // An explicit head read must succeed without falling back from the expired pin.
    for route in ["/api/files", "/api/source?path=a.js"] {
        let (status, body) = call(&app, "GET", &pin_route(route, head), Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["revision"], json!(head));
    }
    assert_eq!(store.status().unwrap().revision, head);
}

#[tokio::test]
async fn browser_claimed_unchanged_completes_done_at_head_without_new_manifest() {
    // Manually publish the first startup reconciliation before POST starts the periodic queue tick.
    let (dir, store, state, app) = setup_without_tick();
    std::fs::write(
        dir.path().join("workspace/a.js"),
        "function unchanged() {}\n",
    )
    .unwrap();
    let owner = state.retained_serving_session().unwrap();
    let options = IndexOptions::new(dir.path().join("workspace"));
    let first =
        trellis::index_coordinator::IndexJobCoordinator::prepare_with_session(&store, None, owner)
            .unwrap()
            .run(&options, &Arc::new(AtomicBool::new(false)), |_| {})
            .unwrap();
    let (code, queued) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{queued}");
    let id = queued["id"].as_str().unwrap();
    let completed = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (_, row) = call(&app, "GET", &format!("/api/jobs/{id}"), Value::Null).await;
            if !row["finishedAt"].is_null() {
                break row;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completed["state"], "done", "{completed}");
    assert_eq!(completed["revision"]["indexRevision"], first.index_revision);
    assert_eq!(store.status().unwrap().revision, first);
    let db =
        rusqlite::Connection::open(store.request_db_path().with_file_name("index.db")).unwrap();
    let manifests: i64 = db
        .query_row("SELECT count(*) FROM revision_documents", [], |r| r.get(0))
        .unwrap();
    let immutable: i64 = db
        .query_row("SELECT count(*) FROM document_versions", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        (manifests, immutable),
        (1, 1),
        "an unchanged browser claim publishes no revision and extracts nothing"
    );
}

#[test]
fn gc_queued_before_candidate_yields_and_daily_stamp_defers_rescan() {
    use trellis::store::topology::{GcStage, TopologyRoots, WorkspaceIdentity};
    let dir = tempfile::tempdir().unwrap();
    let current_root = dir.path().join("current");
    let candidate_root = dir.path().join("candidate");
    let state = dir.path().join("state");
    std::fs::create_dir(&current_root).unwrap();
    std::fs::create_dir(&candidate_root).unwrap();
    let current_store = Store::open_for_tests(&state, &current_root).unwrap();
    // GC observes an existing durable queue, as a daemon would after admission.
    assert_eq!(current_store.settle_foreign_root_requests().unwrap(), 0);
    let candidate_store = Store::open_for_tests(&state, &candidate_root).unwrap();
    let roots = TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"));
    let current = WorkspaceIdentity::discover(Some(&current_root), &current_root).unwrap();
    let candidate = WorkspaceIdentity::discover(Some(&candidate_root), &candidate_root).unwrap();
    let candidate_db = roots.index_db(&candidate);
    drop(candidate_store);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    rusqlite::Connection::open(&candidate_db)
        .unwrap()
        .execute(
            "UPDATE index_metadata SET last_opened_at=?1",
            [now - 31 * 86_400],
        )
        .unwrap();
    let session = current_store.writer_session_for_tests().unwrap();
    let probe = current_store.open_maintenance_queue_probe().unwrap();
    let mut reached = 0;
    let mut hook = |stage| -> anyhow::Result<()> {
        if stage == GcStage::BeforeCandidate {
            reached += 1;
            let queued = current_store
                .enqueue_request(&IndexOptions::new(current_root.clone()), None)
                .unwrap();
            assert_eq!(queued.state, "queued");
            assert_eq!(
                probe.check(),
                trellis::store::MaintenanceQueueState::Pending
            );
            anyhow::bail!("maintenance deferred for publication");
        }
        Ok(())
    };
    let error = roots
        .automatic_gc_at_with_hook(&current, session.writer_guard().unwrap(), now, &mut hook)
        .unwrap_err();
    assert!(error.to_string().contains("maintenance deferred"));
    assert_eq!(reached, 1);
    assert!(
        candidate_db.exists(),
        "no candidate was unlinked after queue priority"
    );
    let mut same_day = 0;
    assert_eq!(
        roots
            .automatic_gc_at_with_hook(&current, session.writer_guard().unwrap(), now, &mut |_| {
                same_day += 1;
                Ok(())
            })
            .unwrap(),
        0
    );
    assert_eq!(same_day, 0, "durable daily stamp forbids same-day rescan");
    let mut next_day = 0;
    let next = roots.automatic_gc_at_with_hook(
        &current,
        session.writer_guard().unwrap(),
        now + 86_400,
        &mut |stage| {
            if stage == GcStage::BeforeCandidate {
                next_day += 1;
                anyhow::bail!("defer again");
            }
            Ok(())
        },
    );
    assert!(next.unwrap_err().to_string().contains("defer again"));
    assert_eq!(
        next_day, 1,
        "unscanned candidate is retried only next eligible day"
    );
}

#[tokio::test]
async fn pele_assets_are_local_and_keep_csp() {
    use sha2::{Digest, Sha256};
    let (_d, _store, _state, app) = setup_without_tick();
    for (route, expected) in [
        (
            "/sequence-pele.js",
            include_bytes!("../web/sequence-pele.js").as_slice(),
        ),
        (
            "/vendor/pele-1.10.0-trellis.2.js",
            include_bytes!("../web/vendor/pele-1.10.0-trellis.2.js").as_slice(),
        ),
        (
            "/treemap.js",
            include_bytes!("../web/treemap.js").as_slice(),
        ),
        (
            "/treemap-pele.js",
            include_bytes!("../web/treemap-pele.js").as_slice(),
        ),
    ] {
        let req = Request::builder()
            .uri(route)
            .header("host", "127.0.0.1:7331")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{route}");
        assert_eq!(
            response.headers()["content-type"],
            "text/javascript; charset=utf-8"
        );
        assert!(
            !response.headers()["content-security-policy"]
                .to_str()
                .unwrap()
                .contains("unsafe-inline")
        );
        assert_eq!(
            to_bytes(response.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .as_ref(),
            expected
        );
    }
    assert_eq!(
        format!(
            "{:x}",
            Sha256::digest(include_bytes!("../web/vendor/pele-1.10.0-trellis.2.js"))
        ),
        "bb4220ed66550152469e0e9a3afb6c532d5812ad62c45126234ed2ca42a0ccea"
    );
}
