use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{fs, sync::atomic::AtomicBool};
#[test]
fn selected_length_guard_precedes_json_and_blob_decode() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("one.js"), "function one() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().into()),
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
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    let (node_id, original): (String, String) = db
        .query_row(
            "SELECT id,payload FROM graph_nodes WHERE path='one.js' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let selected = store.read_revision_for(&db, None, true).unwrap();
    db.execute(
            "UPDATE graph_nodes SET payload=?1 WHERE id=?2 AND projection_id=(SELECT graph_projection_id FROM revision_documents WHERE path='one.js')",
            params!["not-valid-graph-json".repeat(2000), node_id],
        )
        .unwrap();
    assert!(
        store
            .attest_selected_document_for(&db, "one.js", &selected)
            .unwrap_err()
            .to_string()
            .contains("graph row byte budget exceeded")
    );
    db.execute(
            "UPDATE graph_nodes SET payload=?1 WHERE id=?2 AND projection_id=(SELECT graph_projection_id FROM revision_documents WHERE path='one.js')",
            params![original, node_id],
        )
        .unwrap();
    // A tiny test limit exercises the same SQL size gate without a 256 MiB fixture.
    assert!(
        Store::selected_source_row_bounded(&db, "one.js", 4)
            .unwrap_err()
            .to_string()
            .contains("byte budget exceeded")
    );
    db.execute(
            "UPDATE document_versions SET source_bytes=?1,byte_length=length(?1) WHERE id=(SELECT document_version_id FROM revision_documents WHERE path='one.js')",
            ["not-valid-json".repeat(2000)],
        )
        .unwrap();
    assert!(
        Store::selected_source_row_bounded(&db, "one.js", 64)
            .unwrap_err()
            .to_string()
            .contains("byte budget exceeded")
    );
    db.execute(
            "UPDATE document_versions SET source_bytes=x'ff',byte_length=1 WHERE id=(SELECT document_version_id FROM revision_documents WHERE path='one.js')",
            [],
        )
        .unwrap();
    assert!(
        !Store::selected_source_row_bounded(&db, "one.js", 256 * 1024 * 1024)
            .unwrap_err()
            .to_string()
            .contains("byte budget exceeded")
    );
}
