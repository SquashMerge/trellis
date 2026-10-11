#![cfg(feature = "test-support")]
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
    indexer::{IndexOptions, index_workspace},
    model::{CancelFlag, Graph},
    store::{Store, topology::WriterSession},
};
const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
fn fixture() -> (
    tempfile::TempDir,
    Store,
    Graph,
    Router,
    String,
    Arc<WriterSession>,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(
        root.join("a.js"),
        "function seed() { other(); } function other() {}",
    )
    .unwrap();
    let options = IndexOptions::new(root.clone());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let graph = index_workspace(&options, &cancel, |_| {}).unwrap();
    let seed = graph
        .nodes
        .iter()
        .find(|node| node.name == "seed")
        .unwrap()
        .id
        .clone();
    let store = crate::common::open_store(&temp.path().join("state"), &root).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let state = http::new(
        store.clone(),
        options,
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    let app = http::router(state);
    (temp, store, graph, app, seed, session)
}
async fn call(app: &Router, method: &str, path: &str, body: Value) -> (u16, Value) {
    let req = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:7331")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let response = app.clone().oneshot(req).await.unwrap();
    let code = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 2 * 1024 * 1024)
        .await
        .unwrap();
    (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}
fn pinned(route: &str, pin: &trellis::model::IndexPin) -> String {
    let separator = if route.contains('?') { '&' } else { '?' };
    format!(
        "{route}{separator}indexGeneration={}&indexRevision={}",
        pin.index_generation, pin.index_revision
    )
}

#[tokio::test]
async fn durable_crud_matrix() {
    let (temp, store, graph, app, seed, session) = fixture();
    let durable = temp.path().join("state/data/workspaces");
    assert_eq!(
        call(&app, "GET", "/api/views", Value::Null).await.1,
        json!([])
    );
    assert_eq!(
        call(&app, "GET", "/api/annotations", Value::Null).await.1,
        json!([])
    );
    assert_eq!(
        call(&app, "DELETE", "/api/views/absent", Value::Null)
            .await
            .0,
        204
    );
    assert!(
        !durable.exists(),
        "absent reads/deletes do not create durable records"
    );

    let baseline = store.index_baseline().unwrap();
    let pin = publish_bundle(
        &store,
        &graph,
        &temp.path().join("workspace"),
        session.writer_guard().unwrap(),
        baseline,
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let view = json!({"id":"view","title":"Keep","query":{"seed":seed},"pins":{},"hidden":[]});
    let canonical = serde_json::to_value(
        serde_json::from_value::<trellis::model::SavedView>(view.clone()).unwrap(),
    )
    .unwrap();
    assert_eq!(
        call(&app, "PUT", "/api/views/view", view.clone()).await.0,
        400
    );
    let (status, saved) = call(&app, "PUT", &pinned("/api/views/view", &pin), view.clone()).await;
    assert_eq!(status, 200, "{saved}");
    assert_eq!(saved["view"]["id"], "view");
    assert_eq!(saved["view"]["title"], "Keep");
    assert_eq!(saved["view"]["query"], canonical["query"]);
    assert_eq!(saved["view"]["anchor"]["syntaxId"], seed);
    assert_eq!(saved["indexGeneration"], pin.index_generation.to_string());
    assert_eq!(saved["indexRevision"], pin.index_revision);
    assert_eq!(saved["attachment"]["availability"], "ready");
    assert_eq!(saved["attachment"]["result"]["status"], "attached");
    assert_eq!(
        call(&app, "GET", "/api/views/view", Value::Null).await.1["view"],
        saved["view"]
    );

    let note = json!({"id":"note","nodeId":seed,"body":"Preserved"});
    assert_eq!(
        call(&app, "PUT", "/api/annotations/note", note.clone())
            .await
            .0,
        400
    );
    let (status, saved_note) = call(
        &app,
        "PUT",
        &pinned("/api/annotations/note", &pin),
        note.clone(),
    )
    .await;
    assert_eq!(status, 200, "{saved_note}");
    assert_eq!(saved_note["annotation"]["id"], "note");
    assert_eq!(saved_note["annotation"]["nodeId"], seed);
    assert_eq!(saved_note["annotation"]["body"], "Preserved");
    assert_eq!(saved_note["annotation"]["anchor"]["syntaxId"], seed);
    assert_eq!(saved_note["attachment"]["availability"], "ready");
    assert_eq!(
        call(&app, "GET", "/api/annotations", Value::Null).await.1[0]["annotation"],
        saved_note["annotation"]
    );
    assert_eq!(
        call(&app, "PUT", &pinned("/api/views/wrong", &pin), view.clone())
            .await
            .0,
        400
    );
    assert_eq!(
        call(&app, "DELETE", "/api/views/view", Value::Null).await.0,
        204
    );
    assert_eq!(
        call(&app, "DELETE", "/api/annotations/note", Value::Null)
            .await
            .0,
        204
    );
    assert_eq!(
        call(&app, "GET", "/api/views", Value::Null).await.1,
        json!([])
    );
    let record = std::fs::read_dir(&durable)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|p| p.is_dir())
        .unwrap();
    assert!(
        record.join("workspace.db").exists(),
        "empty record remains durable"
    );
}

#[tokio::test]
async fn query_optional_pin_matrix() {
    let (temp, store, graph, app, seed, session) = fixture();
    let root = temp.path().join("workspace");
    let pin = publish_bundle(
        &store,
        &graph,
        &root,
        session.writer_guard().unwrap(),
        store.index_baseline().unwrap(),
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let body = json!({"seed":seed});

    let (status, unpinned) = call(&app, "POST", "/api/query", body.clone()).await;
    assert_eq!(status, 200, "{unpinned}");
    assert_eq!(unpinned["revision"], json!(pin));
    let (status, current) = call(&app, "POST", &pinned("/api/query", &pin), body.clone()).await;
    assert_eq!(status, 200, "{current}");
    assert_eq!(current["revision"], json!(pin));

    for route in [
        format!("/api/query?indexGeneration={}", pin.index_generation),
        format!("/api/query?indexRevision={}", pin.index_revision),
        "/api/query?1".to_owned(),
        format!(
            "/api/query?indexGeneration={}&indexRevision={}&extra=true",
            pin.index_generation, pin.index_revision
        ),
    ] {
        assert_eq!(
            call(&app, "POST", &route, body.clone()).await.0,
            400,
            "{route}"
        );
    }
    let wrong_generation = format!(
        "/api/query?indexGeneration={}&indexRevision={}",
        uuid::Uuid::new_v4(),
        pin.index_revision
    );
    assert_eq!(
        call(&app, "POST", &wrong_generation, body.clone()).await.0,
        409
    );

    let next = publish_bundle(
        &store,
        &graph,
        &root,
        session.writer_guard().unwrap(),
        pin,
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let (status, retained) = call(&app, "POST", &pinned("/api/query", &pin), body.clone()).await;
    assert_eq!(status, 200, "{retained}");
    assert_eq!(retained["revision"], json!(pin));
    let (status, current) = call(&app, "POST", &pinned("/api/query", &next), body.clone()).await;
    assert_eq!(status, 200, "{current}");
    assert_eq!(current["revision"], json!(next));
    let (status, ordinary) = call(&app, "POST", "/api/query", body).await;
    assert_eq!(status, 200, "{ordinary}");
    assert_eq!(ordinary["revision"], json!(next));
}

#[tokio::test]
async fn saved_read_pin_and_ownership_matrix() {
    let (temp, store, graph, app, seed, session) = fixture();
    let root = temp.path().join("workspace");
    let other = graph
        .nodes
        .iter()
        .find(|node| node.name == "other")
        .unwrap()
        .id
        .clone();
    let pin = publish_bundle(
        &store,
        &graph,
        &root,
        session.writer_guard().unwrap(),
        store.index_baseline().unwrap(),
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    let view = json!({"id":"owned","title":"Original","query":{"seed":seed}});
    let (status, created) =
        call(&app, "PUT", &pinned("/api/views/owned", &pin), view.clone()).await;
    assert_eq!(status, 200, "{created}");
    let view_anchor = created["view"]["anchor"].clone();
    assert_eq!(created["attachment"]["result"]["targetId"], seed);

    let note = json!({"id":"owned-note","nodeId":seed,"body":"body","title":"Heading"});
    let (status, created_note) = call(
        &app,
        "PUT",
        &pinned("/api/annotations/owned-note", &pin),
        note.clone(),
    )
    .await;
    assert_eq!(status, 200, "{created_note}");
    let note_anchor = created_note["annotation"]["anchor"].clone();

    for route in [
        format!("/api/views?indexGeneration={}", pin.index_generation),
        format!("/api/views?indexRevision={}", pin.index_revision),
        "/api/views?1".to_owned(),
        format!("/api/annotations?indexRevision={}", pin.index_revision),
    ] {
        assert_eq!(
            call(&app, "GET", &route, Value::Null).await.0,
            400,
            "{route}"
        );
    }
    let (status, listed) = call(&app, "GET", &pinned("/api/views", &pin), Value::Null).await;
    assert_eq!(status, 200, "{listed}");
    assert_eq!(listed[0]["view"]["anchor"], view_anchor);
    assert_eq!(
        listed[0]["indexGeneration"],
        pin.index_generation.to_string()
    );
    assert_eq!(listed[0]["indexRevision"], pin.index_revision);
    let (status, opened) = call(&app, "GET", &pinned("/api/views/owned", &pin), Value::Null).await;
    assert_eq!(status, 200, "{opened}");
    assert_eq!(opened["view"]["anchor"], view_anchor);

    let replacement = json!({"id":"owned","title":"No","query":{"seed":other}});
    assert_eq!(
        call(&app, "PUT", &pinned("/api/views/owned", &pin), replacement)
            .await
            .0,
        400
    );
    let note_replacement = json!({"id":"owned-note","nodeId":other,"body":"No"});
    assert_eq!(
        call(
            &app,
            "PUT",
            &pinned("/api/annotations/owned-note", &pin),
            note_replacement
        )
        .await
        .0,
        400
    );

    let offered_anchor = json!({
        "id":"owned",
        "title":"No",
        "query":{"seed":seed},
        "anchor":view_anchor
    });
    assert_eq!(
        call(
            &app,
            "PUT",
            &pinned("/api/views/owned", &pin),
            offered_anchor
        )
        .await
        .0,
        400
    );
    let offered_metadata = json!({
        "id":"owned-note",
        "nodeId":seed,
        "body":"No",
        "attachment":{"availability":"ready"}
    });
    assert_eq!(
        call(
            &app,
            "PUT",
            &pinned("/api/annotations/owned-note", &pin),
            offered_metadata
        )
        .await
        .0,
        400
    );

    let edit = json!({"id":"owned","title":"Edited","query":{"seed":seed}});
    let (status, edited) = call(&app, "PUT", &pinned("/api/views/owned", &pin), edit).await;
    assert_eq!(status, 200, "{edited}");
    assert_eq!(edited["view"]["anchor"], created["view"]["anchor"]);
    let note_edit = json!({"id":"owned-note","nodeId":seed,"body":"edited"});
    let (status, edited_note) = call(
        &app,
        "PUT",
        &pinned("/api/annotations/owned-note", &pin),
        note_edit,
    )
    .await;
    assert_eq!(status, 200, "{edited_note}");
    assert_eq!(edited_note["annotation"]["anchor"], note_anchor);
    assert_eq!(
        edited_note["annotation"]["title"], "Heading",
        "omitted optional title preserves the stored title"
    );

    let legacy: trellis::model::SavedView = serde_json::from_value(json!({
        "id":"legacy","title":"Legacy","query":{"seed":seed}
    }))
    .unwrap();
    store.put_view(&legacy).unwrap();
    let (status, legacy_state) =
        call(&app, "GET", &pinned("/api/views/legacy", &pin), Value::Null).await;
    assert_eq!(status, 200, "{legacy_state}");
    assert!(legacy_state["view"].get("anchor").is_none());
    assert_eq!(legacy_state["attachment"]["availability"], "anchorless");
    assert!(
        legacy_state["orphanedIds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &json!(seed))
    );
    let legacy_edit = json!({"id":"legacy","title":"Still legacy","query":{"seed":seed}});
    let (status, legacy_edited) =
        call(&app, "PUT", &pinned("/api/views/legacy", &pin), legacy_edit).await;
    assert_eq!(status, 200, "{legacy_edited}");
    assert!(legacy_edited["view"].get("anchor").is_none());
    assert_eq!(legacy_edited["attachment"]["availability"], "anchorless");

    let next = publish_bundle(
        &store,
        &graph,
        &root,
        session.writer_guard().unwrap(),
        pin,
        &Arc::new(AtomicBool::new(false)),
    )
    .unwrap();
    for route in [pinned("/api/views", &pin), pinned("/api/annotations", &pin)] {
        let (status, retained) = call(&app, "GET", &route, Value::Null).await;
        assert_eq!(status, 200, "{route}: {retained}");
        for entry in retained.as_array().unwrap() {
            assert_eq!(entry["indexGeneration"], pin.index_generation.to_string());
            assert_eq!(entry["indexRevision"], json!(pin.index_revision));
        }
    }
    assert_eq!(
        call(&app, "PUT", &pinned("/api/views/owned", &pin), view)
            .await
            .0,
        409
    );
    let (status, current) = call(&app, "GET", &pinned("/api/views", &next), Value::Null).await;
    assert_eq!(status, 200, "{current}");
    assert!(current.as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn missing_anchor_document_keeps_authenticated_saved_routes_listable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.js"), "function stable() { return 1; }\n").unwrap();
    std::fs::write(root.join("b.js"), "function target() { return 2; }\n").unwrap();
    let options = IndexOptions::new(root.clone());
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let store = crate::common::open_store(&temp.path().join("state"), &root).unwrap();
    let session = store.writer_session_for_tests().unwrap();
    let (graph, native, capture) =
        trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
            .unwrap();
    let stable = native
        .declarations
        .iter()
        .find(|row| row.document.path == "a.js" && row.name.as_deref() == Some("stable"))
        .unwrap()
        .syntax_id
        .clone();
    let target = native
        .declarations
        .iter()
        .find(|row| row.document.path == "b.js" && row.name.as_deref() == Some("target"))
        .unwrap()
        .syntax_id
        .clone();
    let first = store
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
        options.clone(),
        TOKEN.into(),
        "127.0.0.1:7331".parse().unwrap(),
    )
    .unwrap();
    state.retain_serving_session(session.clone());
    let app = http::router(state);
    let target_view = json!({"id":"target-view","title":"Target","query":{"seed":target}});
    let stable_view = json!({"id":"stable-view","title":"Stable","query":{"seed":stable}});
    let target_note = json!({"id":"target-note","nodeId":target,"body":"target note"});
    let stable_note = json!({"id":"stable-note","nodeId":stable,"body":"stable note"});
    let (status, created_target_view) = call(
        &app,
        "PUT",
        &pinned("/api/views/target-view", &first),
        target_view,
    )
    .await;
    assert_eq!(status, 200, "{created_target_view}");
    let (status, created_stable_view) = call(
        &app,
        "PUT",
        &pinned("/api/views/stable-view", &first),
        stable_view,
    )
    .await;
    assert_eq!(status, 200, "{created_stable_view}");
    let (status, created_target_note) = call(
        &app,
        "PUT",
        &pinned("/api/annotations/target-note", &first),
        target_note,
    )
    .await;
    assert_eq!(status, 200, "{created_target_note}");
    let (status, created_stable_note) = call(
        &app,
        "PUT",
        &pinned("/api/annotations/stable-note", &first),
        stable_note,
    )
    .await;
    assert_eq!(status, 200, "{created_stable_note}");
    let target_view_raw = store
        .saved_view_at("target-view", Some(first))
        .unwrap()
        .unwrap()
        .view
        .anchor
        .unwrap()
        .get()
        .to_owned();
    let target_note_raw = store
        .saved_annotations_at(Some(first))
        .unwrap()
        .into_iter()
        .find(|state| state.annotation.id == "target-note")
        .unwrap()
        .annotation
        .anchor
        .unwrap()
        .get()
        .to_owned();

    std::fs::remove_file(root.join("b.js")).unwrap();
    let (graph, native, capture) =
        trellis::indexer::index_workspace_bundle(&options, store.root_id(), &cancel, |_| {})
            .unwrap();
    let second = store
        .publish_native(
            &graph,
            &capture,
            &native,
            session.writer_guard().unwrap(),
            first,
            &cancel,
        )
        .unwrap();

    let (status, views) = call(&app, "GET", &pinned("/api/views", &second), Value::Null).await;
    assert_eq!(status, 200, "{views}");
    assert_eq!(views.as_array().unwrap().len(), 2);
    let missing_view = views
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["view"]["id"] == "target-view")
        .unwrap();
    assert_eq!(missing_view["attachment"]["availability"], "ready");
    assert_eq!(missing_view["attachment"]["result"]["status"], "orphaned");
    assert_eq!(missing_view["attachment"]["result"]["reason"], "missing");
    assert_eq!(
        missing_view["view"]["anchor"],
        created_target_view["view"]["anchor"]
    );
    let unaffected_view = views
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["view"]["id"] == "stable-view")
        .unwrap();
    assert_eq!(
        unaffected_view["attachment"]["result"]["status"],
        "attached"
    );
    assert_eq!(
        unaffected_view["view"]["anchor"],
        created_stable_view["view"]["anchor"]
    );
    for (id, expected_status, expected_reason) in [
        ("target-view", "orphaned", "missing"),
        ("stable-view", "attached", "none"),
    ] {
        let route = pinned(&format!("/api/views/{id}"), &second);
        let (status, opened) = call(&app, "GET", &route, Value::Null).await;
        assert_eq!(status, 200, "{opened}");
        assert_eq!(opened["attachment"]["result"]["status"], expected_status);
        assert_eq!(opened["attachment"]["result"]["reason"], expected_reason);
    }

    let (status, notes) = call(
        &app,
        "GET",
        &pinned("/api/annotations", &second),
        Value::Null,
    )
    .await;
    assert_eq!(status, 200, "{notes}");
    assert_eq!(notes.as_array().unwrap().len(), 2);
    let missing_note = notes
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["annotation"]["id"] == "target-note")
        .unwrap();
    assert_eq!(missing_note["attachment"]["availability"], "ready");
    assert_eq!(missing_note["attachment"]["result"]["status"], "orphaned");
    assert_eq!(missing_note["attachment"]["result"]["reason"], "missing");
    assert_eq!(
        missing_note["annotation"]["anchor"],
        created_target_note["annotation"]["anchor"]
    );
    let unaffected_note = notes
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["annotation"]["id"] == "stable-note")
        .unwrap();
    assert_eq!(
        unaffected_note["attachment"]["result"]["status"],
        "attached"
    );
    assert_eq!(
        unaffected_note["annotation"]["anchor"],
        created_stable_note["annotation"]["anchor"]
    );

    let target_view_after = store
        .saved_view_at("target-view", Some(second))
        .unwrap()
        .unwrap();
    assert_eq!(
        target_view_after.view.anchor.as_deref().unwrap().get(),
        target_view_raw
    );
    let target_note_after = store
        .saved_annotations_at(Some(second))
        .unwrap()
        .into_iter()
        .find(|state| state.annotation.id == "target-note")
        .unwrap();
    assert_eq!(
        target_note_after
            .annotation
            .anchor
            .as_deref()
            .unwrap()
            .get(),
        target_note_raw
    );
}

#[tokio::test]
async fn schema5_is_not_native_anchor_evidence() {
    let (_temp, store, _graph, app, seed, _session) = fixture();
    let legacy_pin = store.index_baseline().unwrap();
    let legacy: trellis::model::SavedView = serde_json::from_value(json!({
        "id":"legacy","title":"Legacy","query":{"seed":seed}
    }))
    .unwrap();
    store.put_view(&legacy).unwrap();

    let (status, response) = call(&app, "GET", "/api/views/legacy", Value::Null).await;
    assert_eq!(status, 200, "{response}");
    assert_eq!(response["view"]["query"]["seed"], seed);
    assert!(response["view"].get("anchor").is_none());
    assert!(response["indexGeneration"].is_null());
    assert!(response["indexRevision"].is_null());
    assert_eq!(response["attachment"]["availability"], "indexUnavailable");
    assert!(response["attachment"]["result"].is_null());

    let (status, error) = call(
        &app,
        "GET",
        &pinned("/api/views/legacy", &legacy_pin),
        Value::Null,
    )
    .await;
    assert_eq!(status, 503, "{error}");
    assert_eq!(error["error"]["code"], "index_not_ready");
    let body = json!({"id":"new","title":"No capture","query":{"seed":seed}});
    let (status, error) = call(&app, "PUT", &pinned("/api/views/new", &legacy_pin), body).await;
    assert_eq!(status, 503, "{error}");
    assert_eq!(error["error"]["code"], "index_not_ready");
}

#[tokio::test]
async fn workspace_root_changed() {
    let (temp, store, _graph, app, _seed, _session) = fixture();
    let original = temp.path().join("workspace");
    let moved = temp.path().join("moved");
    std::fs::rename(&original, &moved).unwrap();
    std::fs::create_dir(&original).unwrap();
    for (method, path, body) in [
        ("GET", "/api/status", Value::Null),
        ("GET", "/api/tree", Value::Null),
        ("GET", "/api/files", Value::Null),
        ("GET", "/api/views", Value::Null),
    ] {
        let (code, result) = call(&app, method, path, body).await;
        assert_eq!(code, 409, "{path}: {result}");
        assert_eq!(result["error"]["code"], "root_changed");
    }
    assert!(store.status().is_err());
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

// A bounded, fresh, protected RO snapshot of the selected pin's release markers.
// The use guard and inode witness prevent mistaking a replaced index for this read.
fn retained_release_snapshot(
    state: &std::path::Path,
    root: &std::path::Path,
    pin: &trellis::model::IndexPin,
) -> String {
    use std::{os::unix::fs::MetadataExt, time::Duration};
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};

    let observed = (|| -> anyhow::Result<String> {
        let identity = WorkspaceIdentity::discover(Some(root), root)?;
        let roots = TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"));
        let guard = roots.index_use_existing_readonly(&identity)?;
        let path = roots.index_db(&identity);
        let before = std::fs::symlink_metadata(&path)?;
        anyhow::ensure!(before.is_file() && !before.file_type().is_symlink());
        let db = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::ZERO)?;
        db.pragma_update(None, "query_only", "ON")?;
        db.execute_batch("BEGIN DEFERRED")?;
        let key = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
        let (state, superseded_at, debt, pending, released, generation, revision): (
            Option<String>,
            Option<i64>,
            i64,
            i64,
            i64,
            String,
            i64,
        ) = db.query_row(
            "SELECT
                (SELECT state FROM native_revision_supersessions WHERE revision_id=?1),
                (SELECT superseded_at FROM native_revision_supersessions WHERE revision_id=?1),
                (SELECT count(*) FROM native_revision_release_debt WHERE revision_id=?1),
                (SELECT count(*) FROM revision_capture_inputs
                    WHERE revision_id=?1 AND input_key='__pending_release:v1'),
                (SELECT count(*) FROM revision_capture_inputs
                    WHERE revision_id=?1 AND input_key='__released:v1'),
                index_generation,index_revision
             FROM index_metadata WHERE singleton=1",
            [&key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )?;
        db.execute_batch("ROLLBACK")?;
        drop(db);
        let after = std::fs::symlink_metadata(&path)?;
        guard.verify()?;
        anyhow::ensure!(
            (before.dev(), before.ino()) == (after.dev(), after.ino())
                && after.is_file()
                && !after.file_type().is_symlink()
                && generation.len() <= 36
                && state
                    .as_ref()
                    .is_none_or(|s| matches!(s.as_str(), "retained" | "pending" | "released"))
        );
        Ok(format!(
            "state={state:?},superseded_at={superseded_at:?},debt={debt},pending_marker={pending},released_marker={released},head_generation={generation},head_revision={revision},index_inode={}:{}",
            after.dev(),
            after.ino()
        ))
    })();
    observed.unwrap_or_else(|_| "snapshot=unknown".to_owned())
}

#[tokio::test]
async fn saved_views_and_annotations_attach_to_the_requested_retained_manifest() {
    let (temp, store, graph, app, seed, session) = fixture();
    let root = temp.path().join("workspace");
    let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
    let r1 = publish_bundle(
        &store,
        &graph,
        &root,
        session.writer_guard().unwrap(),
        store.index_baseline().unwrap(),
        &cancel,
    )
    .unwrap();
    let view = json!({"id":"old-view","title":"Old","query":{"seed":seed},"pins":{},"hidden":[]});
    let annotation = json!({"id":"old-note","nodeId":seed,"body":"Measured"});
    let (status, attached_view) =
        call(&app, "PUT", &pinned("/api/views/old-view", &r1), view).await;
    assert_eq!(status, 200, "{attached_view}");
    let (status, attached_note) = call(
        &app,
        "PUT",
        &pinned("/api/annotations/old-note", &r1),
        annotation,
    )
    .await;
    assert_eq!(status, 200, "{attached_note}");
    assert_eq!(attached_view["attachment"]["result"]["status"], "attached");
    assert_eq!(attached_note["attachment"]["result"]["status"], "attached");
    std::fs::write(root.join("a.js"), "function replacement() {}\n").unwrap();
    let updated = index_workspace(&IndexOptions::new(root.clone()), &cancel, |_| {}).unwrap();
    store.set_retention_clock_for_tests(1_000, 0);
    let r2 = publish_bundle(
        &store,
        &updated,
        &root,
        session.writer_guard().unwrap(),
        r1,
        &cancel,
    )
    .unwrap();
    assert_ne!(r1, r2);
    let (code, old_view) = call(
        &app,
        "GET",
        &pinned("/api/views/old-view", &r1),
        Value::Null,
    )
    .await;
    assert_eq!(code, 200, "{old_view}");
    assert_eq!(old_view, attached_view);
    let (code, old_notes) = call(&app, "GET", &pinned("/api/annotations", &r1), Value::Null).await;
    assert_eq!(code, 200, "{old_notes}");
    let matching_notes: Vec<_> = old_notes
        .as_array()
        .unwrap()
        .iter()
        .filter(|state| state["annotation"]["id"] == "old-note")
        .collect();
    assert_eq!(matching_notes.len(), 1, "{old_notes}");
    assert_eq!(matching_notes[0], &attached_note);
    let current_views = call(&app, "GET", "/api/views", Value::Null).await.1;
    assert_ne!(
        current_views[0]["attachment"]["result"]["status"],
        "attached"
    );
    store.set_retention_clock_for_tests(1_900, 900);
    let release_before = retained_release_snapshot(&temp.path().join("state"), &root, &r1);
    let maintenance_busy_before = store.maintenance_sqlite_busy_attempts();
    let release_started = std::time::Instant::now();
    let release_deadline = std::time::Duration::from_secs(5);
    let mut release_deferred = Vec::new();
    let mut release_attempts = 0;
    let mut released = false;
    for attempt in 1..=32 {
        if release_started.elapsed() >= release_deadline {
            break;
        }
        release_attempts = attempt;
        match store.release_revision(r1, session.writer_guard().unwrap()) {
            Ok(()) => {
                released = true;
                break;
            }
            Err(error)
                if error
                    .downcast_ref::<trellis::store::SqliteContention>()
                    .is_some() =>
            {
                release_deferred.push(format!(
                    "attempt={attempt}:{}",
                    retained_release_snapshot(&temp.path().join("state"), &root, &r1)
                ));
            }
            Err(_) => panic!(
                "release returned an unexpected non-contention error;attempts={attempt};before=({release_before});deferred={release_deferred:?}"
            ),
        }
        if attempt < 32 && release_started.elapsed() < release_deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
    assert!(
        released,
        "release did not succeed within 32 attempts/5s;attempts={release_attempts};before=({release_before});deferred={release_deferred:?};after=({});queue={};maintenance_busy={maintenance_busy_before}->{}",
        retained_release_snapshot(&temp.path().join("state"), &root, &r1),
        store
            .open_maintenance_queue_probe()
            .map(|probe| format!("{:?}", probe.check()))
            .unwrap_or_else(|_| "unknown".to_owned()),
        store.maintenance_sqlite_busy_attempts()
    );
    let release_after = retained_release_snapshot(&temp.path().join("state"), &root, &r1);
    let maintenance_busy_after = store.maintenance_sqlite_busy_attempts();
    let queue_after = store
        .open_maintenance_queue_probe()
        .map(|probe| format!("{:?}", probe.check()))
        .unwrap_or_else(|_| "unknown".to_owned());
    // Public read-only telemetry does not expose permit or step state.
    let (code, expired_view) = call(
        &app,
        "GET",
        &pinned("/api/views/old-view", &r1),
        Value::Null,
    )
    .await;
    assert_eq!(
        code, 409,
        "release_attempts={release_attempts};deferred={release_deferred:?};release_before=({release_before});release_after=({release_after});maintenance_busy={maintenance_busy_before}->{maintenance_busy_after};queue_after={queue_after};permit=unknown;step=unknown"
    );
    assert_eq!(expired_view["error"]["code"], "pin_expired");
    let (code, expired_notes) =
        call(&app, "GET", &pinned("/api/annotations", &r1), Value::Null).await;
    assert_eq!(
        code, 409,
        "release_attempts={release_attempts};deferred={release_deferred:?};release_before=({release_before});release_after=({release_after});maintenance_busy={maintenance_busy_before}->{maintenance_busy_after};queue_after={queue_after};permit=unknown;step=unknown"
    );
    assert_eq!(expired_notes["error"]["code"], "pin_expired");
}
