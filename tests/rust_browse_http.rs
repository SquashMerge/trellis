//! Rust file -> methods -> sequence on cached source, without executing the workspace.
mod common;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use tower::ServiceExt;
use trellis::{
    http,
    indexer::{IndexOptions, index_workspace_bundle},
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
async fn request(app: &Router, method: &str, path: &str, body: Value) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("host", "127.0.0.1:7331")
                .header("authorization", format!("Bearer {TOKEN}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn rust_methods_sequence_and_source_survive_live_file_removal() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname='untrusted_fixture'\nversion='0.1.0'\nbuild='build.rs'\n",
    )
    .unwrap();
    let sentinel = temp.path().join("MUST_NOT_RUN");
    std::fs::write(
        workspace.join("build.rs"),
        format!(
            "fn main() {{ std::fs::write({:?}, \"executed\").unwrap(); }}",
            sentinel.to_str().unwrap()
        ),
    )
    .unwrap();
    let text = "pub struct Counter; impl Counter { pub fn increment(&self, x: i32) -> i32 { if x > 0 { save(x); } x + 1 } } fn save(x: i32) { audit(x); }\n";
    std::fs::write(workspace.join("lib.rs"), text).unwrap();
    std::fs::write(
        workspace.join("helper.js"),
        "function helper() { notify(); }",
    )
    .unwrap();
    let options = IndexOptions::new(workspace.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = crate::common::open_store(&temp.path().join("state"), &workspace).unwrap();
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(workspace.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert!(!sentinel.exists());
    assert!(
        graph
            .files
            .iter()
            .any(|f| f.path == "lib.rs" && f.language == "rust")
    );
    assert!(
        graph
            .files
            .iter()
            .any(|f| f.path == "helper.js" && f.language == "javascript")
    );
    let session = store.leader_session().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.leader_guard().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let pin = store.status().unwrap().revision;
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    let app = http::router(state);
    let (code, tree) = request(&app, "GET", "/api/tree", Value::Null).await;
    assert_eq!(code, 200);
    let entry = tree["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "lib.rs")
        .unwrap();
    assert_eq!(entry["indexedPath"], "lib.rs");
    assert!(entry["unindexedReason"].is_null());
    let (code, methods) = request(
        &app,
        "GET",
        &format!(
            "/api/methods?path=lib.rs&indexGeneration={}&indexRevision={}",
            pin.index_generation, pin.index_revision
        ),
        Value::Null,
    )
    .await;
    assert_eq!(code, 200);
    let method = methods["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["symbol"]["name"] == "increment")
        .unwrap();
    assert_eq!(method["symbol"]["kind"], "method");
    let seed = method["symbol"]["id"].as_str().unwrap();
    std::fs::remove_file(workspace.join("lib.rs")).unwrap();
    let (code, sequence) = request(
        &app,
        "POST",
        "/api/sequence",
        json!({"seed":seed,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(sequence["seed"]["id"], seed);
    assert!(!sequence["steps"].as_array().unwrap().is_empty());
    let measured = graph
        .calls
        .iter()
        .find(|c| c.caller == seed && c.callee_text.as_deref() == Some("save"))
        .unwrap();
    assert!(sequence.to_string().contains(&measured.id));
    let (code, source) = request(
        &app,
        "GET",
        &format!(
            "/api/source?path=lib.rs&indexGeneration={}&indexRevision={}",
            pin.index_generation, pin.index_revision
        ),
        Value::Null,
    )
    .await;
    assert_eq!(code, 200);
    assert_eq!(source["file"]["text"], text);
    // A new publication needs a live root again; the previous reads used cached source.
    std::fs::write(workspace.join("lib.rs"), text).unwrap();
    let (refreshed_graph, _refreshed_native, _refreshed_capture) = index_workspace_bundle(
        &IndexOptions::new(workspace.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    assert_eq!(refreshed_graph.files, graph.files);
    assert_eq!(refreshed_graph.calls, graph.calls);
    // The live daemon owns publication after retention. Queue the next
    // revision through it rather than opening a second writer while its tick
    // may have an active SQLite transaction.
    let (code, job) = request(&app, "POST", "/api/index", json!({})).await;
    assert_eq!(code, 202, "{job}");
    let id = job["id"].as_str().unwrap();
    let completed = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let (code, result) =
                request(&app, "GET", &format!("/api/jobs/{id}"), Value::Null).await;
            assert_eq!(code, 200, "{result}");
            if !result["finishedAt"].is_null() {
                break result;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(completed["state"], "done", "{completed}");
    assert_eq!(
        completed["revision"]["indexGeneration"],
        pin.index_generation.to_string()
    );
    assert!(
        completed["revision"]["indexRevision"]
            .as_u64()
            .is_some_and(|revision| revision > pin.index_revision),
        "queue did not publish a successor: {completed}"
    );
    std::fs::remove_file(workspace.join("lib.rs")).unwrap();
    let (code, retained) = request(
        &app,
        "POST",
        "/api/sequence",
        json!({"seed":seed,"expectedRevision":pin}),
    )
    .await;
    assert_eq!(code, 200, "{retained}");
    assert_eq!(retained["revision"], json!(pin));
    assert!(!sentinel.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_tick_keeps_pinned_and_current_source_available_across_fifty_edits() {
    use std::time::{Duration, Instant};
    use trellis::index_coordinator;

    struct StopLoad(Arc<AtomicBool>);
    impl Drop for StopLoad {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let source = workspace.join("lib.rs");
    std::fs::write(&source, "pub fn value() -> i32 { -1 }\n").unwrap();
    let options = IndexOptions::new(workspace.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = common::open_store(&temp.path().join("state"), &workspace).unwrap();
    let session =
        index_coordinator::establish_serving_session(&store, Some(&options), &cancel).unwrap();
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session);
    let app = http::router(state.clone());
    let load_stop = StopLoad(Arc::new(AtomicBool::new(false)));
    let mut load_thread = None;
    for edit in 0..50 {
        if edit == 25 {
            let stop = load_stop.0.clone();
            load_thread = Some(std::thread::spawn(move || {
                let mut value = 1_u64;
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    for _ in 0..2_000 {
                        value = value.wrapping_mul(6364136223846793005).wrapping_add(1);
                    }
                    std::hint::black_box(value);
                    std::thread::yield_now();
                }
            }));
        }
        let previous = store.status().unwrap().revision;
        let text = format!("pub fn value() -> i32 {{ {edit} }}\n");
        std::fs::write(&source, &text).unwrap();
        // Deliver the change as the OS watcher would. Publication then does not
        // depend on notification latency under load; a late native event is a
        // harmless duplicate (#131).
        let modified = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Data(notify::event::DataChange::Content),
        ))
        .add_path(source.clone());
        let delivered_by = Instant::now() + Duration::from_secs(10);
        while !state.submit_watch_event_for_tests(modified.clone()) {
            assert!(
                Instant::now() < delivered_by,
                "the queue tick never started its scheduler"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let (status_code, status) = request(&app, "GET", "/api/status", Value::Null).await;
            assert_eq!(status_code, 200, "edit {edit}: {status}");
            let (pin_code, pinned) = request(
                &app,
                "GET",
                &format!(
                    "/api/source?path=lib.rs&indexGeneration={}&indexRevision={}",
                    previous.index_generation, previous.index_revision
                ),
                Value::Null,
            )
            .await;
            assert_eq!(
                pin_code, 200,
                "edit {edit}, previous {previous:?}: {pinned}"
            );
            let (current_code, current) =
                request(&app, "GET", "/api/source?path=lib.rs", Value::Null).await;
            assert_eq!(current_code, 200, "edit {edit}: {current}");
            if status["revision"]["indexRevision"]
                .as_u64()
                .is_some_and(|revision| revision > previous.index_revision)
                && current["file"]["text"] == text
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "edit {edit} was not published: {status}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    load_stop
        .0
        .store(true, std::sync::atomic::Ordering::Release);
    load_thread.unwrap().join().unwrap();
}
