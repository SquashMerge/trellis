#![cfg(feature = "test-support")]
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use tower::ServiceExt;
use trellis::{
    dependencies::CatalogOptions,
    http,
    indexer::{IndexOptions, index_workspace_bundle},
    model::Graph,
    store::Store,
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Fixture {
    temp: tempfile::TempDir,
    store: Store,
    state: Arc<http::DaemonState>,
    app: Router,
    session: Arc<trellis::store::topology::WriterSession>,
}
fn setup(enabled: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let library = temp.path().join("library");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::create_dir_all(library.join("std/src")).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = 'app'\nversion = '0.1.0'\nedition = '2021'\n",
    )
    .unwrap();
    std::fs::write(workspace.join("src/lib.rs"), "pub fn workspace_only() {}\n").unwrap();
    std::fs::write(
        library.join("std/Cargo.toml"),
        "[package]\nname = 'std'\nversion = '0.0.0'\n",
    )
    .unwrap();
    std::fs::write(library.join("std/src/lib.rs"), "// café\npub struct LibraryType;\nimpl LibraryType { pub fn method(&self) { hidden_call(); } }\npub enum Other { A }\n").unwrap();
    let store = crate::common::open_store(&temp.path().join("state"), &workspace).unwrap();
    assert!(store.status().is_err());
    // Match daemon admission: publish the virgin queue before the writer's
    // shared index-use session and first startup reconciliation, so a later request can be ACKed.
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(workspace.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
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
    let state = http::new_with_dependency_options(
        store.clone(),
        IndexOptions::new(workspace.clone()),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
        workspace,
        vec![],
        enabled.then_some(CatalogOptions {
            cargo_home: None,
            rust_library: Some(library),
        }),
    )
    .unwrap();
    let app = http::router(state.clone());
    Fixture {
        temp,
        store,
        state,
        app,
        session,
    }
}
async fn request(app: &Router, method: &str, path: &str, body: &str) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
async fn ready(app: &Router) -> Value {
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (status, body) = request(app, "GET", "/api/dependencies", "").await;
            assert_eq!(status, 200);
            if body["state"] != "loading" {
                assert_eq!(body["state"], "ready", "{body}");
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("dependency build timed out")
}
fn symbol_url(catalog: &Value) -> String {
    format!(
        "/api/dependencies/symbols?catalogId={}",
        catalog["catalogId"].as_str().unwrap()
    )
}
async fn source_url(app: &Router, catalog: &Value) -> String {
    let (_, symbols) = request(app, "GET", &symbol_url(catalog), "").await;
    let symbol = symbols["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "LibraryType")
        .unwrap();
    format!(
        "/api/dependencies/source?catalogId={}&sourceRef={}",
        catalog["catalogId"].as_str().unwrap(),
        symbol["sourceRef"].as_str().unwrap()
    )
}
#[tokio::test]
async fn startup_catalog_is_separate_paged_and_source_is_explicit() {
    let fixture = setup(true);
    let before = serde_json::to_value(fixture.store.status().unwrap()).unwrap();
    fixture.state.start_dependency_index();
    let status = ready(&fixture.app).await;
    assert!(status["symbolCount"].as_u64().unwrap() >= 3);
    assert!(status.get("symbols").is_none());
    assert!(!status.to_string().contains("hidden_call"));
    assert!(
        !status
            .to_string()
            .contains(fixture.temp.path().to_str().unwrap())
    );
    let package = status["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "std")
        .unwrap();
    let (_, page) = request(
        &fixture.app,
        "GET",
        &format!(
            "{}&packageId={}&limit=1",
            symbol_url(&status),
            package["id"].as_str().unwrap()
        ),
        "",
    )
    .await;
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["nextOffset"], 1);
    let (_, filtered) = request(
        &fixture.app,
        "GET",
        &format!("{}&q=libraryTYPE", symbol_url(&status)),
        "",
    )
    .await;
    assert!(
        filtered["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "LibraryType")
    );
    for symbol in filtered["items"].as_array().unwrap() {
        assert!(
            !symbol["signature"]
                .as_str()
                .unwrap()
                .contains("hidden_call")
        );
    }
    let source = source_url(&fixture.app, &status).await;
    let (code, snapshot) = request(&fixture.app, "GET", &source, "").await;
    assert_eq!(code, 200, "{snapshot}");
    assert_eq!(snapshot["hash"], snapshot["file"]["hash"]);
    let definition = snapshot["definitions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["name"] == "LibraryType")
        .unwrap();
    let text = snapshot["file"]["text"].as_str().unwrap();
    assert_eq!(
        &text[definition["range"]["startByte"].as_u64().unwrap() as usize
            ..definition["range"]["endByte"].as_u64().unwrap() as usize],
        "pub struct LibraryType;"
    );
    assert_eq!(
        serde_json::to_value(fixture.store.status().unwrap()).unwrap(),
        before
    );
    let external_id = definition["id"].as_str().unwrap();
    assert_eq!(
        request(
            &fixture.app,
            "POST",
            "/api/sequence",
            &json!({"seed":external_id,"expectedRevision":fixture.store.status().unwrap().revision}).to_string()
        )
        .await
        .0,
        404
    );
    let (code, workspace_classes) =
        request(&fixture.app, "GET", "/api/classes?q=LibraryType", "").await;
    assert_eq!(code, 200, "{workspace_classes}");
    assert!(workspace_classes["items"].as_array().unwrap().is_empty());
}
#[tokio::test]
async fn source_hash_and_workspace_revision_reject_stale_reads() {
    let fixture = setup(true);
    fixture.state.start_dependency_index();
    let catalog = ready(&fixture.app).await;
    let source = source_url(&fixture.app, &catalog).await;
    std::fs::write(
        fixture.temp.path().join("library/std/src/lib.rs"),
        "pub struct Changed;\n",
    )
    .unwrap();
    let (code, response) = request(&fixture.app, "GET", &source, "").await;
    assert_eq!(code, 409);
    assert!(!response.to_string().contains("Changed"));
    assert_eq!(
        request(&fixture.app, "POST", "/api/dependencies/refresh", "{}")
            .await
            .0,
        202
    );
    let replacement = ready(&fixture.app).await;
    assert_ne!(replacement["catalogId"], catalog["catalogId"]);
    assert_eq!(
        request(&fixture.app, "GET", &symbol_url(&catalog), "")
            .await
            .0,
        409
    );
    let before = fixture.store.index_baseline().unwrap();
    let error = fixture
        .store
        .publish(
            &Graph::default(),
            fixture.session.writer_guard().unwrap(),
            before,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("native_evidence_required"),
        "{error}"
    );
    assert_eq!(fixture.store.index_baseline().unwrap(), before);
    let workspace = fixture.temp.path().join("workspace");
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(workspace),
        fixture.store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    fixture
        .store
        .publish_native(
            &graph,
            &capture,
            &native,
            fixture.session.writer_guard().unwrap(),
            before,
            &cancel,
        )
        .unwrap();
    assert_eq!(
        request(&fixture.app, "GET", &symbol_url(&replacement), "")
            .await
            .0,
        409
    );
    let (_, status) = request(&fixture.app, "GET", "/api/dependencies", "").await;
    assert_eq!(status["state"], "failed");
    assert_eq!(
        status["workspaceRevision"],
        serde_json::json!(fixture.store.status().unwrap().revision)
    );
    assert!(status["catalogId"].is_null());
    assert_eq!(
        request(&fixture.app, "POST", "/api/dependencies/refresh", "{}")
            .await
            .0,
        202
    );
    assert_eq!(
        ready(&fixture.app).await["workspaceRevision"],
        serde_json::json!(fixture.store.status().unwrap().revision)
    );
}
#[tokio::test]
async fn request_validation_and_capability_only_access() {
    let fixture = setup(true);
    fixture.state.start_dependency_index();
    let catalog = ready(&fixture.app).await;
    for body in [
        "",
        "null",
        "[]",
        "[{}]",
        "{\"path\":\"/etc\"}",
        "{\"force\":true}",
        "{\"expectedRevision\":0}",
    ] {
        assert_eq!(
            request(&fixture.app, "POST", "/api/dependencies/refresh", body)
                .await
                .0,
            400,
            "{body}"
        );
    }
    for suffix in [
        "&limit=0",
        "&limit=201",
        "&offset=50001",
        "&root=/etc",
        "&path=secret.rs",
    ] {
        assert_eq!(
            request(&fixture.app, "GET", &(symbol_url(&catalog) + suffix), "")
                .await
                .0,
            400
        );
    }
    let id = catalog["catalogId"].as_str().unwrap();
    for source_ref in [
        "unknown",
        "%2Fetc%2Fpasswd",
        "..%2Fsecret",
        "std/src/lib.rs",
    ] {
        assert_eq!(
            request(
                &fixture.app,
                "GET",
                &format!("/api/dependencies/source?catalogId={id}&sourceRef={source_ref}"),
                ""
            )
            .await
            .0,
            404
        );
    }
    let source = source_url(&fixture.app, &catalog).await;
    assert_eq!(
        request(&fixture.app, "GET", &(source + "&path=/etc/passwd"), "")
            .await
            .0,
        400
    );
}
#[tokio::test]
async fn all_dependency_endpoints_require_auth_host_and_origin() {
    let fixture = setup(false);
    for (method, path) in [
        ("GET", "/api/dependencies"),
        ("GET", "/api/dependencies/symbols?catalogId=x"),
        ("GET", "/api/dependencies/source?catalogId=x&sourceRef=x"),
        ("POST", "/api/dependencies/refresh"),
    ] {
        for (host, origin, token, expected) in [
            ("127.0.0.1:7331", None, None, 401),
            ("evil.test", None, Some(TOKEN), 403),
            ("127.0.0.1:7331", Some("http://evil.test"), Some(TOKEN), 403),
        ] {
            let mut req = Request::builder()
                .method(method)
                .uri(path)
                .header("host", host);
            if let Some(origin) = origin {
                req = req.header("origin", origin);
            }
            if let Some(token) = token {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            assert_eq!(
                fixture
                    .app
                    .clone()
                    .oneshot(req.body(Body::from("{}")).unwrap())
                    .await
                    .unwrap()
                    .status(),
                expected
            );
        }
    }
}
#[tokio::test]
async fn disabled_and_shutdown_do_not_launch_catalog_work() {
    let fixture = setup(false);
    fixture.state.start_dependency_index();
    assert_eq!(
        request(&fixture.app, "GET", "/api/dependencies", "")
            .await
            .1["state"],
        "disabled"
    );
    assert_eq!(
        request(&fixture.app, "POST", "/api/dependencies/refresh", "{}")
            .await
            .0,
        409
    );
    let old_state = http::new(
        fixture.store.clone(),
        IndexOptions::new(fixture.temp.path().join("workspace")),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    old_state.start_dependency_index();
    assert_eq!(
        request(&http::router(old_state), "GET", "/api/dependencies", "")
            .await
            .1["state"],
        "disabled"
    );
    let fixture = setup(true);
    // No scheduler yield between starting and shutdown: cancelled generation cannot publish.
    fixture.state.start_dependency_index();
    fixture.state.cancel_active();
    fixture.state.start_dependency_index();
    assert_eq!(
        request(&fixture.app, "POST", "/api/dependencies/refresh", "{}")
            .await
            .0,
        503
    );
    let (_, status) = request(&fixture.app, "GET", "/api/dependencies", "").await;
    assert_eq!(status["state"], "failed");
    assert!(status["catalogId"].is_null());
}
#[tokio::test]
async fn successful_workspace_index_automatically_rebuilds_catalog() {
    let fixture = setup(true);
    fixture.state.start_dependency_index();
    let old = ready(&fixture.app).await;
    fixture
        .state
        .retain_serving_session(fixture.session.clone());
    // The catalog is keyed by the workspace revision. An unchanged index keeps
    // the revision (Decision 0006 §3), so edit a source to publish a new one.
    std::fs::write(
        fixture.temp.path().join("workspace/src/lib.rs"),
        "pub fn workspace_only() {}\npub fn edited() {}\n",
    )
    .unwrap();
    assert_eq!(
        request(&fixture.app, "POST", "/api/index", "{}").await.0,
        202
    );
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (_, job) = request(&fixture.app, "GET", "/api/jobs/current", "").await;
            if job["state"] == "done" {
                break;
            }
            assert_ne!(job["state"], "failed", "{job}");
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let expected = serde_json::json!(fixture.store.status().unwrap().revision);
    let new = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (status, body) = request(&fixture.app, "GET", "/api/dependencies", "").await;
            assert_eq!(status, 200);
            assert_eq!(body["workspaceRevision"], expected);
            if body["state"] == "ready" {
                assert_ne!(old["catalogId"], body["catalogId"]);
                break body;
            }
            assert!(
                body["state"] == "loading"
                    || (body["state"] == "failed"
                        && body["catalogId"].is_null()
                        && body["warnings"]
                            == serde_json::json!([
                                "Workspace changed; refresh the dependency catalog"
                            ])),
                "{body}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("automatic dependency refresh timed out");
    assert_eq!(new["workspaceRevision"], expected);
}
#[cfg(unix)]
#[tokio::test]
async fn sources_keep_pinned_roots_and_reject_symlink_replacement() {
    let fixture = setup(true);
    fixture.state.start_dependency_index();
    let catalog = ready(&fixture.app).await;
    let source = source_url(&fixture.app, &catalog).await;
    let library = fixture.temp.path().join("library");
    let moved = fixture.temp.path().join("moved-library");
    std::fs::rename(&library, &moved).unwrap();
    std::fs::create_dir_all(library.join("std/src")).unwrap();
    std::fs::write(
        library.join("std/src/lib.rs"),
        "replacement must not be read",
    )
    .unwrap();
    assert_eq!(request(&fixture.app, "GET", &source, "").await.0, 200);
    std::fs::remove_file(moved.join("std/src/lib.rs")).unwrap();
    std::os::unix::fs::symlink(library.join("std/src/lib.rs"), moved.join("std/src/lib.rs"))
        .unwrap();
    let (status, response) = request(&fixture.app, "GET", &source, "").await;
    assert_eq!(status, 403);
    assert!(!response.to_string().contains("replacement"));
}

#[tokio::test]
async fn workspace_generation_reuse() {
    use trellis::store::topology::UseGuard;
    let Fixture {
        temp,
        store,
        state,
        app,
        session,
    } = setup(true);
    state.start_dependency_index();
    let catalog = ready(&app).await;
    let old = store.status().unwrap().revision;
    let source = source_url(&app, &catalog).await;
    let indexes = temp.path().join("state/cache/indexes");
    let index = std::fs::read_dir(&indexes)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|p| p.is_dir())
        .unwrap();
    let lock = indexes.join(format!(
        "{}.lock",
        index.file_name().unwrap().to_string_lossy()
    ));
    drop(session);
    let exclusive = UseGuard::acquire_existing(&lock, true, true).unwrap();
    // Simulate loss of disposable index evidence, not GC of the durable
    // request queue: the same-root queue and its use lock remain in place.
    std::fs::remove_file(index.join("index.db")).unwrap();
    drop(exclusive);
    let replacement =
        crate::common::open_store(&temp.path().join("state"), &temp.path().join("workspace"))
            .unwrap();
    assert!(replacement.status().is_err());
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(temp.path().join("workspace")),
        replacement.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let replacement_session = replacement.writer_session_for_tests().unwrap();
    let fresh = replacement
        .publish_native(
            &graph,
            &capture,
            &native,
            replacement_session.writer_guard().unwrap(),
            replacement.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    assert_eq!(fresh.index_revision, old.index_revision);
    assert_ne!(fresh.index_generation, old.index_generation);
    assert_eq!(request(&app, "GET", &symbol_url(&catalog), "").await.0, 409);
    assert_eq!(request(&app, "GET", &source, "").await.0, 409);
    let (_, status) = request(&app, "GET", "/api/dependencies", "").await;
    assert_eq!(status["workspaceRevision"], serde_json::json!(fresh));
    assert_eq!(status["state"], "failed");
}
