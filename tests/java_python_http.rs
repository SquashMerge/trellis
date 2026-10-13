#![cfg(feature = "test-support")]
//! Synthetic Java/Python file -> method -> static sequence integration.
//! Workspace code, build scripts, and Python imports must never execute.
mod common;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use tower::ServiceExt;
use trellis::{
    behavior::{SequenceStep, SequenceView},
    http,
    indexer::{IndexOptions, index_workspace},
    model::{Graph, SymbolKind},
    store::{
        SqliteContention, Store,
        topology::{IndexNotReady, WriterSession},
    },
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const JAVA: &str = r#"interface Port { void signatureOnly(); }
abstract class Worker {
    abstract void abstractOnly();
    native void nativeOnly();
    Worker() { setup(); }
    int run(int value) {
        if (value > 0) { save(value); }
        return finish(value);
    }
}
"#;
const PYTHON: &str = r#"class Worker:
    def run(self, value):
        if value > 0:
            save(value)
        return finish(value)

    def empty(self):
        pass
"#;

struct Fixture {
    temp: tempfile::TempDir,
    workspace: PathBuf,
    sentinel: PathBuf,
    store: Store,
    pin: trellis::model::IndexPin,
    graph: Graph,
    app: Router,
    state: Arc<http::DaemonState>,
    session: Arc<WriterSession>,
}
fn setup() -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let sentinel = temp.path().join("MUST_NOT_EXECUTE");
    // A Gradle configuration and executable wrapper tripwire, not a real build.
    std::fs::write(
        workspace.join("build.gradle"),
        format!(
            "new File({:?}).text = 'executed'\n",
            sentinel.to_str().unwrap()
        ),
    )
    .unwrap();
    let wrapper = workspace.join("gradlew");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf executed > '{}'\nexit 99\n",
            sentinel.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    // Importing this source would create the sentinel. Parsing it must not.
    std::fs::write(
        workspace.join("worker.py"),
        format!(
            "from pathlib import Path\nPath({:?}).write_text('executed')\n{}",
            sentinel.to_str().unwrap(),
            PYTHON
        ),
    )
    .unwrap();
    for (path, text) in [
        ("Worker.java", JAVA),
        ("helper.js", "function helper() { notify(); }\n"),
        ("helper.rs", "fn helper() { notify(); }\n"),
        ("unsupported.ts", "function unsupported(): void {}\n"),
        ("README.md", "Synthetic source-only integration fixture\n"),
    ] {
        std::fs::write(workspace.join(path), text).unwrap();
    }
    let options = IndexOptions::new(workspace.clone());
    let cancel = Arc::new(AtomicBool::new(false));
    let graph = index_workspace(&options, &cancel, |_| {}).unwrap();
    assert!(!sentinel.exists(), "indexing executed workspace code");
    let store = crate::common::open_store(&temp.path().join("state"), &workspace).unwrap();
    store.settle_foreign_root_requests().unwrap();
    let session = store.writer_session_for_tests().unwrap();
    assert_eq!(
        publish_bundle(
            &store,
            &graph,
            &workspace,
            session.writer_guard().unwrap(),
            trellis::model::IndexPin {
                index_generation: store.index_baseline().unwrap().index_generation,
                index_revision: 0
            },
            &cancel
        )
        .unwrap()
        .index_revision,
        1
    );
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    let app = http::router(state.clone());
    Fixture {
        temp,
        workspace,
        sentinel,
        pin: store.status().unwrap().revision,
        store,
        graph,
        app,
        state,
        session,
    }
}
fn pinned(f: &Fixture, path: &str) -> String {
    let join = if path.contains('?') { '&' } else { '?' };
    format!(
        "{path}{join}indexGeneration={}&indexRevision={}",
        f.pin.index_generation, f.pin.index_revision
    )
}
async fn call(app: &Router, method: &str, path: &str, body: Value) -> (u16, Value) {
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
    assert_eq!(response.headers()["cache-control"], "no-store");
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
fn flatten<'a>(steps: &'a [SequenceStep], out: &mut Vec<&'a SequenceStep>) {
    for step in steps {
        out.push(step);
        flatten(&step.children, out);
        flatten(&step.alternate, out);
    }
}

#[tokio::test]
async fn mixed_catalog_tree_and_lexical_parents_remain_language_neutral() {
    let f = setup();
    let (code, files) = call(&f.app, "GET", &pinned(&f, "/api/files"), Value::Null).await;
    assert_eq!(code, 200);
    let (code, tree) = call(&f.app, "GET", "/api/tree", Value::Null).await;
    assert_eq!(code, 200);
    for (path, language) in [
        ("Worker.java", "java"),
        ("worker.py", "python"),
        ("helper.js", "javascript"),
        ("helper.rs", "rust"),
    ] {
        let count = f
            .graph
            .nodes
            .iter()
            .filter(|n| {
                n.path == path && matches!(n.kind, SymbolKind::Function | SymbolKind::Method)
            })
            .count();
        assert!(count > 0, "missing methods for {path}");
        let file = files["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap();
        assert_eq!(file["language"], language);
        assert_eq!(file["methodCount"], count);
        let entry = tree["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap();
        assert_eq!(entry["indexedPath"], path);
        assert_eq!(entry["methodCount"], count);
        assert!(entry.get("unindexedReason").is_none());
        let (code, methods) = call(
            &f.app,
            "GET",
            &pinned(&f, &format!("/api/methods?path={path}")),
            Value::Null,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(methods["items"].as_array().unwrap().len(), count);
        for item in methods["items"].as_array().unwrap() {
            let symbol = &item["symbol"];
            let indexed = f.graph.nodes.iter().find(|n| n.id == symbol["id"]).unwrap();
            assert_eq!(symbol, &serde_json::to_value(indexed).unwrap());
            if symbol["name"] == "run" {
                let parent = f
                    .graph
                    .nodes
                    .iter()
                    .find(|n| Some(&n.id) == indexed.parent.as_ref())
                    .unwrap();
                assert_eq!(parent.kind, SymbolKind::Class);
                assert_eq!(parent.name, "Worker");
                assert_eq!(parent.path, path);
            }
        }
    }
    assert_eq!(files["items"].as_array().unwrap().len(), 4);
    // Supported but not in the published snapshot is not an unsupported language.
    std::fs::write(f.workspace.join("Pending.java"), "class Pending {}\n").unwrap();
    std::fs::write(f.workspace.join("pending.py"), "pass\n").unwrap();
    let (_, tree) = call(&f.app, "GET", "/api/tree", Value::Null).await;
    for (path, reason) in [
        (
            "Pending.java",
            "Not indexed yet (may be excluded or size-limited)",
        ),
        (
            "pending.py",
            "Not indexed yet (may be excluded or size-limited)",
        ),
        ("unsupported.ts", "TypeScript indexing not supported yet"),
        ("README.md", "Unsupported source type"),
    ] {
        let entry = tree["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["path"] == path)
            .unwrap();
        assert!(entry["indexedPath"].is_null());
        assert_eq!(entry["unindexedReason"], reason);
    }
    assert!(!f.sentinel.exists());
}

#[tokio::test]
async fn cached_java_python_sequences_keep_measured_calls_after_source_deletion() {
    fn expected_commit_barrier(error: &anyhow::Error) -> bool {
        error.chain().any(|cause| {
            cause.downcast_ref::<IndexNotReady>().is_some()
                || cause.downcast_ref::<SqliteContention>().is_some()
                || matches!(cause.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::SqliteFailure(info, _))
                        if matches!(info.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
        })
    }
    let f = setup();
    for path in ["Worker.java", "worker.py"] {
        let seed = f
            .graph
            .nodes
            .iter()
            .find(|n| n.path == path && n.name == "run")
            .unwrap();
        let request = json!({"seed":seed.id,"expectedRevision":f.pin});
        let before = call(&f.app, "POST", "/api/sequence", request.clone()).await;
        assert_eq!(before.0, 200, "{}", before.1);
        let sequence: SequenceView = serde_json::from_value(before.1.clone()).unwrap();
        assert_eq!(sequence.seed, *seed);
        let mut steps = Vec::new();
        flatten(&sequence.steps, &mut steps);
        assert!(steps.iter().filter_map(|s| s.call_id.as_ref()).count() >= 2);
        for step in steps.iter().filter(|s| s.call_id.is_some()) {
            let measured = f
                .graph
                .calls
                .iter()
                .find(|c| Some(&c.id) == step.call_id.as_ref())
                .unwrap();
            assert_eq!(measured.caller, seed.id, "no foreign method body calls");
            assert_eq!(step.range, measured.range);
            assert_eq!(step.path, measured.path);
            assert!(step.resolution.is_none() && step.target.is_none());
            assert!(
                serde_json::to_value(measured)
                    .unwrap()
                    .get("target")
                    .is_none()
            );
            if let Some(target) = &step.target {
                let participant = sequence
                    .participants
                    .iter()
                    .find(|p| &p.id == target)
                    .unwrap();
                assert!(matches!(
                    participant.kind.as_str(),
                    "unresolvedReceiver" | "unresolvedCallee" | "boundary"
                ));
            }
            assert!(
                step.children.is_empty() && step.alternate.is_empty(),
                "unresolved invocation must stay terminal"
            );
        }
        // Observe the real watcher commit for THIS deletion before asserting
        // historical reads. An in-flight same-generation publication may
        // temporarily close public reads; it is never an HTTP success result.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let prior_head = loop {
            match f.store.status() {
                Ok(status) => break status.revision,
                Err(error) if expected_commit_barrier(&error) => {}
                Err(error) => panic!("unexpected selected-head error before deletion: {error:#}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "selected head did not become ready before deletion"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        };
        assert_eq!(prior_head.index_generation, f.pin.index_generation);
        assert!(prior_head.index_revision >= f.pin.index_revision);
        let deleted_path = f.workspace.join(path);
        std::fs::remove_file(&deleted_path).unwrap();
        // A deletion may be coalesced by the OS watcher under all-target load.
        // Submit one accepted hint through the daemon's real watcher ingress.
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let hint =
                    notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::File))
                        .add_path(deleted_path.clone());
                if f.state.submit_watch_event_for_tests(Ok(hint)) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("daemon watcher did not register for deletion");
        loop {
            match f.store.status() {
                Ok(status) => {
                    let head = status.revision;
                    assert_eq!(head.index_generation, prior_head.index_generation);
                    if head.index_revision > prior_head.index_revision {
                        match f.store.source_at(path, Some(head)) {
                            Ok(None) => break,
                            Ok(Some(_)) => {} // The selected head has not yet accounted for this deletion.
                            Err(error) if expected_commit_barrier(&error) => {}
                            Err(error) => {
                                panic!("unexpected selected-source error after deletion: {error:#}")
                            }
                        }
                    }
                }
                Err(error) if expected_commit_barrier(&error) => {}
                Err(error) => panic!("unexpected selected-head error after deletion: {error:#}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "watcher did not commit a new head without deleted source"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(call(&f.app, "POST", "/api/sequence", request).await, before);
        let (code, source) = call(
            &f.app,
            "GET",
            &pinned(&f, &format!("/api/source?path={path}")),
            Value::Null,
        )
        .await;
        assert_eq!(code, 200);
        assert_eq!(
            source["file"]["text"],
            f.graph
                .files
                .iter()
                .find(|file| file.path == path)
                .unwrap()
                .text
        );
        assert_eq!(
            call(
                &f.app,
                "GET",
                &pinned(&f, &format!("/api/methods?path={path}")),
                Value::Null
            )
            .await
            .0,
            200
        );
        let (code, all) = call(
            &f.app,
            "POST",
            "/api/sequence",
            json!({"seed":seed.id,"expectedRevision":f.pin,"showAll":true}),
        )
        .await;
        assert_eq!(code, 200);
        let all: SequenceView = serde_json::from_value(all).unwrap();
        let mut all_steps = Vec::new();
        flatten(&all.steps, &mut all_steps);
        for step in all_steps.iter().filter(|s| s.call_id.is_some()) {
            assert!(step.resolution.is_none() && step.target.is_none());
            assert!(step.children.is_empty() && step.alternate.is_empty());
            if let Some(target) = &step.target {
                let participant = all.participants.iter().find(|p| &p.id == target).unwrap();
                assert!(matches!(
                    participant.kind.as_str(),
                    "unresolvedReceiver" | "unresolvedCallee" | "boundary"
                ));
            }
        }
    }
    assert!(!f.sentinel.exists());
    assert!(f.temp.path().join("state").is_dir());
}

#[tokio::test]
async fn declarations_without_bodies_are_navigable_without_fabricated_calls() {
    let f = setup();
    let (code, methods) = call(
        &f.app,
        "GET",
        &pinned(&f, "/api/methods?path=Worker.java"),
        Value::Null,
    )
    .await;
    assert_eq!(code, 200);
    for name in ["signatureOnly", "abstractOnly", "nativeOnly"] {
        let method = methods["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["symbol"]["name"] == name)
            .unwrap();
        let seed = method["symbol"]["id"].as_str().unwrap();
        let (code, value) = call(
            &f.app,
            "POST",
            "/api/sequence",
            json!({"seed":seed,"expectedRevision":f.pin}),
        )
        .await;
        assert_eq!(code, 200, "{name}: {value}");
        let sequence: SequenceView = serde_json::from_value(value).unwrap();
        let mut steps = Vec::new();
        flatten(&sequence.steps, &mut steps);
        assert!(steps.iter().all(|s| s.call_id.is_none()));
        assert!(!f.graph.calls.iter().any(|c| c.caller == seed));
        let explanation = format!("{:?} {:?}", sequence.warnings, sequence.steps).to_lowercase();
        assert!(
            explanation.contains("body"),
            "bodyless declaration must be explicit: {explanation}"
        );
    }
    let empty = f
        .graph
        .nodes
        .iter()
        .find(|n| n.path == "worker.py" && n.name == "empty")
        .unwrap();
    let sequence = f
        .store
        .sequence_at(&empty.id, f.store.status().unwrap().revision, false)
        .unwrap()
        .unwrap();
    let mut steps = Vec::new();
    flatten(&sequence.steps, &mut steps);
    assert!(steps.iter().all(|s| s.call_id.is_none()));
    assert!(!f.sentinel.exists());
}

#[tokio::test]
async fn java_python_cached_endpoints_enforce_revision_and_auth() {
    let f = setup();
    let cancel = Arc::new(AtomicBool::new(false));
    let index_generation = f.store.status().unwrap().revision.index_generation;
    assert_eq!(
        publish_bundle(
            &f.store,
            &f.graph,
            &f.workspace,
            f.session.writer_guard().unwrap(),
            trellis::model::IndexPin {
                index_generation,
                index_revision: 1
            },
            &cancel
        )
        .unwrap()
        .index_revision,
        2
    );
    for path in ["Worker.java", "worker.py"] {
        let seed = &f
            .graph
            .nodes
            .iter()
            .find(|n| n.path == path && n.name == "run")
            .unwrap()
            .id;
        for endpoint in [
            pinned(&f, &format!("/api/methods?path={path}")),
            pinned(&f, &format!("/api/source?path={path}")),
            pinned(&f, "/api/files"),
        ] {
            let (status, retained) = call(&f.app, "GET", &endpoint, Value::Null).await;
            assert_eq!(status, 200, "{endpoint}: {retained}");
            assert_eq!(retained["revision"], json!(f.pin), "{endpoint}");
        }
        let (status, retained_sequence) = call(
            &f.app,
            "POST",
            "/api/sequence",
            json!({"seed":seed,"expectedRevision":f.pin}),
        )
        .await;
        assert_eq!(status, 200, "{retained_sequence}");
        assert_eq!(retained_sequence["revision"], json!(f.pin));
        assert_eq!(
            call(
                &f.app,
                "POST",
                "/api/sequence",
                json!({"seed":seed,"expectedRevision":f.store.status().unwrap().revision})
            )
            .await
            .0,
            200
        );
        for (method, endpoint) in [
            ("GET", format!("/api/methods?path={path}")),
            ("GET", format!("/api/source?path={path}")),
            ("POST", "/api/sequence".into()),
            ("GET", "/api/files".into()),
            ("GET", "/api/tree".into()),
        ] {
            let response = f
                .app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(endpoint)
                        .header("host", "127.0.0.1:7331")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({"seed":seed,"expectedRevision":f.store.status().unwrap().revision}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), 401);
        }
    }
    assert!(!f.sentinel.exists());
}

fn publish_bundle(
    store: &trellis::store::Store,
    graph: &trellis::model::Graph,
    workspace: &std::path::Path,
    leader: &trellis::store::topology::WriterGuard,
    expected: trellis::model::IndexPin,
    cancel: &trellis::model::CancelFlag,
) -> anyhow::Result<trellis::model::IndexPin> {
    let (indexed, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(workspace.to_owned()),
        store.root_id(),
        cancel,
        |_| {},
    )?;
    assert_eq!(
        &indexed, graph,
        "published graph must match captured source"
    );
    store.publish_native(&indexed, &capture, &native, leader, expected, cancel)
}
