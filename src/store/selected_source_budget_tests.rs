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
            "SELECT n.id,n.name FROM graph_nodes n JOIN revision_documents m
             ON m.graph_projection_id=n.projection_id WHERE m.path='one.js' LIMIT 1",
            [], |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
    let selected = store.read_revision_for(&db, None, true).unwrap();
    let document = store.selected_graph_document_for(&db, "one.js", &selected).unwrap();
    let normal_charge = derived_graph_bytes(&document.nodes, &document.graph_calls,
        &document.graph_regions, document.file.text.len() as i64).unwrap();
    let manual_charge: usize = document.nodes.iter().map(|node|
        node.id.len() + node.name.len() + node.path.len() + json(node).unwrap().len()).sum::<usize>()
        + document.graph_calls.iter().map(|call|
            call.id.len() + call.caller.len() + call.path.len() + json(call).unwrap().len()).sum::<usize>()
        + document.graph_regions.iter().map(|region|
            region.id.len() + region.owner.len() + region.path.len() + json(region).unwrap().len()).sum::<usize>();
    assert_eq!(normal_charge, manual_charge as u64);
    // Use ordinary-size, individually valid labels. The aggregate graph budget
    // must accept its boundary and reject the next byte; no oversized label is
    // allowed by native graph validation (display labels are at most 256 bytes).
    let source_bytes = document.file.text.len() as i64;
    let cap = (source_bytes as usize) * 64 + 256 * 1024;
    let mut template = document.nodes[0].clone();
    template.display_label = Some(String::new());
    let base = template.id.len() + template.name.len() + template.path.len()
        + json(&template).unwrap().len();
    let count = cap / base;
    assert!(count <= (source_bytes * 16).clamp(1024, 1_000_000) as usize);
    assert!((cap - count * base) <= count * 256);
    let mut boundary: Vec<_> = (0..count).map(|i| {
        let mut node = template.clone();
        node.id = format!("sid:v1:{i:032x}");
        node
    }).collect();
    let mut remaining = cap - count * base;
    for node in &mut boundary {
        let size = remaining.min(256);
        node.display_label = Some("x".repeat(size));
        remaining -= size;
    }
    assert_eq!(remaining, 0);
    assert_eq!(derived_graph_bytes(&boundary, &[], &[], source_bytes).unwrap(), cap as u64);
    let last = boundary.iter_mut().find(|node| node.display_label.as_ref().unwrap().len() < 256)
        .unwrap();
    last.display_label.as_mut().unwrap().push('x');
    assert!(derived_graph_bytes(&boundary, &[], &[], source_bytes)
        .unwrap_err().to_string().contains("graph row byte budget exceeded"));
    db.execute(
        "UPDATE graph_nodes SET name=?1 WHERE id=?2 AND projection_id=(SELECT graph_projection_id FROM revision_documents WHERE path='one.js')",
        params!["wrong", node_id],
    ).unwrap();
    assert!(store.attest_selected_document_for(&db, "one.js", &selected)
        .unwrap_err().to_string().contains("graph node materialization differs"));
    db.execute(
        "UPDATE graph_nodes SET name=?1 WHERE id=?2 AND projection_id=(SELECT graph_projection_id FROM revision_documents WHERE path='one.js')",
        params![original, node_id],
    ).unwrap();
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
