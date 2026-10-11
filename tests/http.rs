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
/// Tests that drive maintenance with `force_retention_idle_tick_for_tests`
/// run without the background queue tick. A concurrent tick holding the native
/// stream or serving session makes the forced tick defer silently (#131).
fn setup_without_tick() -> (tempfile::TempDir, Store, Arc<http::DaemonState>, Router) {
    setup_with_tick(false)
}
fn setup_with_tick(tick: bool) -> (tempfile::TempDir, Store, Arc<http::DaemonState>, Router) {
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
    if tick {
        state.retain_serving_session(store.leader_session().unwrap());
    } else {
        state.retain_serving_session_without_tick_for_tests(store.leader_session().unwrap());
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
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
#[tokio::test]
async fn guards_public_and_private() {
    let (_d, _store, _state, app) = setup();
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
        call(&app, "POST", "/api/query", json!({"seed":"x","depth":6}))
            .await
            .0,
        400
    );
    assert_eq!(
        call(
            &app,
            "PUT",
            "/api/views/test",
            json!({"id":"other","title":"v","query":{"seed":"x"}})
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
    let failed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
    let (_d, store, state, app) = setup();
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
                .leader_guard()
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
    let (dir, store, state, app) = setup();
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
                .leader_guard()
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
    let completed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
    assert_eq!(
        store.status().unwrap().revision.index_revision,
        2,
        "mandatory H r1 precedes accepted Q r2 DONE"
    );
    let (_, cancelled) = call(&app, "POST", &format!("/api/jobs/{id}/cancel"), Value::Null).await;
    assert_eq!(cancelled["state"], "done");
    assert_eq!(
        store.status().unwrap().revision.index_revision,
        2,
        "mandatory H r1 precedes accepted Q r2 DONE"
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
    let held = store.leader_session().unwrap();
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
    let done = tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
    let (d, store, _state, app) = setup();
    let source = (0..15000)
        .map(|i| format!("function f{i}() {{ console.log({i}); }}\n"))
        .collect::<String>();
    std::fs::write(d.path().join("workspace/large.js"), source).unwrap();
    let (code, j) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(code, 202);
    let id = j["id"].as_str().unwrap();
    let (code, _) = call(&app, "POST", &format!("/api/jobs/{id}/cancel"), Value::Null).await;
    assert_eq!(code, 409);
    // The 15k-function index runs beside other verifier lanes in CI. This is a
    // bounded completion guard, not an index-latency requirement.
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(60), async {
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
    assert_eq!(
        store.index_baseline().unwrap().index_revision,
        2,
        "accepted Q publishes its own r2 after mandatory H r1; cancellation does not add a revision"
    );
}

#[tokio::test]
async fn local_design_assets_preserve_same_origin_guards() {
    let (_d, _store, _state, app) = setup();
    for (path, mime) in [
        ("/shell.js", "text/javascript; charset=utf-8"),
        ("/fonts/jetbrains-mono-latin.woff2", "font/woff2"),
        ("/fonts/space-grotesk-latin.woff2", "font/woff2"),
        ("/fonts/JetBrainsMono-OFL.txt", "text/plain; charset=utf-8"),
        ("/fonts/SpaceGrotesk-OFL.txt", "text/plain; charset=utf-8"),
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
        assert!(!csp.contains("unsafe-inline"));
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        if mime == "font/woff2" {
            assert_eq!(&bytes[..4], b"wOF2");
        } else if path.ends_with("OFL.txt") {
            assert!(String::from_utf8_lossy(&bytes).contains("SIL OPEN FONT LICENSE"));
        }
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
    assert_eq!(
        call(&app, "GET", "/fonts/missing.woff2", Value::Null)
            .await
            .0,
        404
    );
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
        let symbol_id = graph.nodes[0].id.clone();
        let pin = store
            .publish_native(
                &graph,
                &capture,
                &native,
                state
                    .retained_serving_session()
                    .unwrap()
                    .leader_guard()
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
                    "/api/symbol?id={symbol_id}&indexGeneration={}&indexRevision={}",
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
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
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
    let (_dir, store, state, app, index, roots, identity, old) =
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
    assert_eq!(done["revision"]["indexRevision"], 2);
    assert_ne!(
        done["revision"]["indexGeneration"],
        json!(old)["indexGeneration"]
    );
    assert!(state.retained_serving_session().unwrap().is_leader());
    assert_eq!(
        store.status().unwrap().revision,
        serde_json::from_value(done["revision"].clone()).unwrap()
    );
    assert!(
        roots
            .leader(&identity)
            .unwrap_err()
            .to_string()
            .contains("storage_busy")
    );
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
    assert!(state.retained_serving_session().is_ok());
    assert_eq!(std::fs::read(&index).unwrap(), bytes);
    let reader = roots.index_use_existing(&identity).unwrap();
    let (status, admitted) = call(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(status, StatusCode::ACCEPTED, "{admitted}");
    let id = admitted["id"].as_str().unwrap();
    let queue = store.request_db_path();
    let accepted_bytes = std::fs::read(&queue).unwrap();
    // Poll at the queue tick cadence; CI may schedule its blocking worker late.
    tokio::time::timeout(std::time::Duration::from_secs(15), async {
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
        done["revision"]["indexRevision"], 2,
        "recreation and claimed request publish exactly once each"
    );
    assert_ne!(
        done["revision"]["indexGeneration"],
        json!(old)["indexGeneration"]
    );
    assert!(state.retained_serving_session().unwrap().is_leader());
    assert!(
        roots
            .leader(&identity)
            .unwrap_err()
            .to_string()
            .contains("storage_busy")
    );
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
    assert!(state.retained_serving_session().unwrap().is_leader());
}

#[tokio::test]
async fn released_matching_pin_is_typed_http_conflict_without_head_fallback() {
    let (dir, store, state, app) = setup();
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
                session.leader_guard().unwrap(),
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
        .release_revision(old, session.leader_guard().unwrap())
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
    let (dir, store, state, app) = setup();
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
                session.leader_guard().unwrap(),
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
async fn expired_pin_saved_items_require_explicit_head_reattachment() {
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
                session.leader_guard().unwrap(),
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
    let view = json!({"id":"saved-view","title":"Saved", "query":{"seed":seed}});
    let note = json!({"id":"saved-note","nodeId":seed,"body":"Durable note"});
    for (route, payload) in [
        ("/api/views/saved-view", &view),
        ("/api/annotations/saved-note", &note),
    ] {
        let (status, saved) = call(&app, "PUT", &pin_route(route, old), payload.clone()).await;
        assert_eq!(status, StatusCode::OK, "{saved}");
        assert_eq!(saved["attachment"]["result"]["status"], "attached");
    }
    let head = publish(old); // identical source and measured IDs, but a different revision.
    assert_eq!(store.status().unwrap().revision, head);
    store.set_retention_clock_for_tests(1_900, 900);
    state.force_retention_idle_tick_for_tests().unwrap();
    for (method, route, payload) in [
        ("GET", "/api/source?path=a.js", Value::Null),
        ("GET", "/api/symbol?id=missing", Value::Null),
        ("GET", "/api/files", Value::Null),
        ("GET", "/api/methods?path=a.js", Value::Null),
        ("GET", "/api/classes", Value::Null),
        ("POST", "/api/query", json!({"seed":seed})),
        ("GET", "/api/views", Value::Null),
        ("GET", "/api/views/saved-view", Value::Null),
        ("GET", "/api/annotations", Value::Null),
        ("PUT", "/api/views/saved-view", view.clone()),
        ("PUT", "/api/annotations/saved-note", note.clone()),
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
    let (status, body) = call(
        &app,
        "POST",
        "/api/questions/preview",
        json!({"seed":seed,"question":"What does seed do?", "expectedRevision":old}),
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
        for route in ["/api/views/saved-view", "/api/annotations"] {
            let (status, body) = call(&app, "GET", &pin_route(route, invalid), Value::Null).await;
            assert_eq!(status, StatusCode::CONFLICT, "{route}: {body}");
            assert_eq!(
                body["error"]["code"], "revision_conflict",
                "{route}: {body}"
            );
        }
    }
    // An explicit head read, not a retry at the expired pin, is the only reattachment.
    for (route, expected) in [
        ("/api/views/saved-view", &view),
        ("/api/annotations", &note),
    ] {
        let (status, body) = call(&app, "GET", &pin_route(route, head), Value::Null).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let attached = if route == "/api/annotations" {
            &body[0]
        } else {
            &body
        };
        assert_eq!(
            attached["attachment"]["result"]["status"], "attached",
            "{body}"
        );
        let durable = if route == "/api/annotations" {
            &attached["annotation"]
        } else {
            &attached["view"]
        };
        assert_eq!(durable["id"], expected["id"]);
        assert_eq!(attached["indexRevision"], head.index_revision);
    }
    assert_eq!(store.status().unwrap().revision, head);
}

#[tokio::test]
async fn browser_claimed_unchanged_publishes_new_manifest_before_done() {
    let (dir, store, state, app) = setup();
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
    let completed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
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
    assert_eq!(
        completed["revision"]["indexRevision"],
        first.index_revision + 1
    );
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
        (2, 1),
        "browser claim must publish its own revision without extracting again"
    );
}

/// A separate CLI-like process commits Q2 to durable requests.db. Its stdout
/// is a barrier: the parent never releases maintenance before Q2 is accepted.
#[test]
#[ignore]
fn maintenance_external_fifo_child() {
    use std::io::Write;
    let root = std::path::PathBuf::from(std::env::var_os("TRELLIS_MAINTENANCE_ROOT").unwrap());
    let state = std::path::PathBuf::from(std::env::var_os("TRELLIS_MAINTENANCE_STATE").unwrap());
    let store = Store::open_for_tests(&state, &root).unwrap();
    let queued = store
        .enqueue_request(&IndexOptions::new(root), None)
        .unwrap();
    println!("MAINTENANCE_REQUEST_ID={}", queued.id);
    std::io::stdout().flush().unwrap();
}

fn external_maintenance_request(root: &std::path::Path, state: &std::path::Path) -> String {
    use std::io::BufRead;
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "maintenance_external_fifo_child",
            "--nocapture",
        ])
        .env("TRELLIS_MAINTENANCE_ROOT", root)
        .env("TRELLIS_MAINTENANCE_STATE", state)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut request = String::new();
    for line in std::io::BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if let Some(id) = line.strip_prefix("MAINTENANCE_REQUEST_ID=") {
            request = id.to_owned();
        }
    }
    assert!(child.wait().unwrap().success());
    assert!(!request.is_empty());
    request
}

#[test]
fn queued_second_process_preempts_maintenance_before_writer_admission() {
    use std::sync::mpsc;
    use trellis::index_coordinator::{self, IndexJobCoordinator};
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let data = dir.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("one.js"), "function before() {}\n").unwrap();
    let options = IndexOptions::new(workspace.clone());
    let store = Store::open_for_tests(&data, &workspace).unwrap();
    store.set_retention_clock_for_tests(1000, 0);
    let cancel = Arc::new(AtomicBool::new(false));
    let session =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let old = store.status().unwrap().revision;
    std::fs::write(workspace.join("one.js"), "function after() {}\n").unwrap();
    IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session_without_tick_for_tests(session.clone());
    store.set_retention_clock_for_tests(1899, 899);
    state.force_retention_idle_tick_for_tests().unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    store.set_maintenance_before_writer_hook_for_tests(move || {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    store.set_retention_clock_for_tests(1900, 900);
    let worker = state.clone();
    let maintenance = std::thread::spawn(move || worker.force_retention_idle_tick_for_tests());
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let id = external_maintenance_request(&workspace, &data);
    let queue_store = store.clone();
    let queue_session = session.clone();
    let foreground = std::thread::spawn(move || {
        index_coordinator::drain_one_request_observed(&queue_store, &queue_session, |_, _| {})
    });
    release_tx.send(()).unwrap();
    maintenance.join().unwrap().unwrap();
    assert_eq!(foreground.join().unwrap().unwrap(), 1);
    assert_eq!(store.request_by_id(&id).unwrap().unwrap().state, "done");
    // The older pin survives: maintenance was unable to start its writer
    // after Q2 reached the durable queue, while Q2 published and ACKed.
    assert!(store.source_at("one.js", Some(old)).unwrap().is_some());
}

#[test]
fn second_process_fifo_interrupts_after_first_real_maintenance_delete() {
    use std::sync::mpsc;
    use trellis::index_coordinator::{self, IndexJobCoordinator};
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().join("workspace");
    let data = dir.path().join("state");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("one.js"), "function before() {}\n").unwrap();
    let options = IndexOptions::new(workspace.clone());
    let store = Store::open_for_tests(&data, &workspace).unwrap();
    store.set_retention_clock_for_tests(1000, 0);
    let cancel = Arc::new(AtomicBool::new(false));
    let session =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let old = store.status().unwrap().revision;
    std::fs::write(workspace.join("one.js"), "function after() {}\n").unwrap();
    IndexJobCoordinator::prepare_with_session(&store, None, session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session_without_tick_for_tests(session.clone());
    store.set_retention_clock_for_tests(1899, 899);
    state.force_retention_idle_tick_for_tests().unwrap();
    store.set_retention_clock_for_tests(1900, 900);
    state.force_retention_idle_tick_for_tests().unwrap(); // retained -> pending
    let path = store.request_db_path().with_file_name("index.db");
    let count = || -> i64 {
        let db = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.query_row("SELECT count(*) FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id WHERE r.published_index_revision=?1", [old.index_revision as i64], |row| row.get(0)).unwrap()
    };
    let before = count();
    assert!(before > 0, "pending transition must preserve full manifest");
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    store.set_maintenance_after_first_delete_hook_for_tests(move || {
        entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    let worker = state.clone();
    let maintenance = std::thread::spawn(move || worker.force_retention_idle_tick_for_tests());
    entered_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let id = external_maintenance_request(&workspace, &data);
    release_tx.send(()).unwrap();
    maintenance.join().unwrap().unwrap();
    assert_eq!(
        count(),
        before,
        "post-DELETE queue arrival must ROLLBACK the full maintenance unit"
    );
    let queue_store = store.clone();
    let queue_session = session.clone();
    assert_eq!(
        std::thread::spawn(move || index_coordinator::drain_one_request_observed(
            &queue_store,
            &queue_session,
            |_, _| {},
        ))
        .join()
        .unwrap()
        .unwrap(),
        1
    );
    assert_eq!(store.request_by_id(&id).unwrap().unwrap().state, "done");
    // Debt remains durable. A later genuinely idle owner resumes in one unit.
    state.force_retention_idle_tick_for_tests().unwrap();
    assert!(count() < before);
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
    let session = current_store.leader_session().unwrap();
    let probe = current_store.open_maintenance_queue_probe().unwrap();
    let mut reached = 0;
    let mut hook = |stage| -> anyhow::Result<()> {
        if stage == GcStage::BeforeCandidate {
            reached += 1;
            let _id = external_maintenance_request(&current_root, &state);
            assert_ne!(probe.check(), trellis::store::MaintenanceQueueState::Clear);
            anyhow::bail!("maintenance deferred for publication");
        }
        Ok(())
    };
    let error = roots
        .automatic_gc_at_with_hook(&current, session.leader_guard().unwrap(), now, &mut hook)
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
            .automatic_gc_at_with_hook(&current, session.leader_guard().unwrap(), now, &mut |_| {
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
        session.leader_guard().unwrap(),
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
