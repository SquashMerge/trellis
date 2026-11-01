use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{fs, sync::atomic::AtomicBool};

use crate::evidence_budget::graph_bytes as derived_graph_bytes;

#[test]
fn derived_graph_row_byte_budget_accepts_under_and_rejects_over() {
    let (_state, _root, store) = super::selected_read_once_tests::published_store(1);
    let db = Connection::open(store.roots.index_db(&store.identity)).unwrap();
    let selected = store.read_revision_for(&db, None, true).unwrap();
    let document = store
        .selected_graph_document_for(&db, "f0.js", &selected)
        .unwrap();
    let source_bytes = document.file.text.len() as i64;
    let max_row = (source_bytes as u64)
        .saturating_mul(6)
        .saturating_add(16 * 1024)
        .min(256 * 1024 * 1024 + 16 * 1024);
    let mut node = document.nodes[0].clone();
    node.name = String::new();
    node.display_label = Some(String::new());
    let base = node.id.len() + node.name.len() + node.path.len() + json(&node).unwrap().len();
    assert!(base < max_row as usize);
    // ASCII name bytes count in both the name column and compact JSON; label
    // bytes count only in JSON. Use the label for the final one-byte boundary.
    let missing = max_row as usize - 1 - base;
    node.name = "x".repeat(missing / 2);
    node.display_label = Some("x".repeat(missing % 2));
    let charge = |node: &Symbol| {
        node.id.len() + node.name.len() + node.path.len() + json(node).unwrap().len()
    };
    assert_eq!(charge(&node), max_row as usize - 1);
    assert_eq!(
        derived_graph_bytes(&[node.clone()], &[], &[], source_bytes).unwrap(),
        (max_row - 1)
    );
    node.display_label.as_mut().unwrap().push('x');
    assert_eq!(charge(&node), max_row as usize);
    assert_eq!(
        derived_graph_bytes(&[node.clone()], &[], &[], source_bytes).unwrap(),
        max_row
    );
    node.display_label.as_mut().unwrap().push('x');
    assert_eq!(charge(&node), max_row as usize + 1);
    assert!(
        derived_graph_bytes(&[node], &[], &[], source_bytes)
            .unwrap_err()
            .to_string()
            .contains("graph row byte budget exceeded")
    );
}

/// The source-length gate precedes its decode; derived graph guards run after
/// the selected source and native rows have been decoded.
#[test]
fn selected_source_length_guard_precedes_decode_and_graph_guard_follows() {
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
    let selected = store.read_revision_for(&db, None, true).unwrap();
    let document = store
        .selected_graph_document_for(&db, "one.js", &selected)
        .unwrap();
    let normal_charge = derived_graph_bytes(
        &document.nodes,
        &document.graph_calls,
        &document.graph_regions,
        document.file.text.len() as i64,
    )
    .unwrap();
    let manual_charge: usize = document
        .nodes
        .iter()
        .map(|node| node.id.len() + node.name.len() + node.path.len() + json(node).unwrap().len())
        .sum::<usize>()
        + document
            .graph_calls
            .iter()
            .map(|call| {
                call.id.len() + call.caller.len() + call.path.len() + json(call).unwrap().len()
            })
            .sum::<usize>()
        + document
            .graph_regions
            .iter()
            .map(|region| {
                region.id.len()
                    + region.owner.len()
                    + region.path.len()
                    + json(region).unwrap().len()
            })
            .sum::<usize>();
    assert_eq!(normal_charge, manual_charge as u64);
    // Use ordinary-size, individually valid labels. The aggregate graph budget
    // must accept its boundary and reject the next byte; no oversized label is
    // allowed by native graph validation (display labels are at most 256 bytes).
    let source_bytes = document.file.text.len() as i64;
    let cap = (source_bytes as usize) * 64 + 256 * 1024;
    let mut template = document.nodes[0].clone();
    template.display_label = Some(String::new());
    let base = template.id.len()
        + template.name.len()
        + template.path.len()
        + json(&template).unwrap().len();
    let count = cap / base;
    assert!(count <= (source_bytes * 16).clamp(1024, 1_000_000) as usize);
    assert!((cap - count * base) <= count * 256);
    let mut boundary: Vec<_> = (0..count)
        .map(|i| {
            let mut node = template.clone();
            node.id = format!("sid:v1:{i:032x}");
            node
        })
        .collect();
    let mut remaining = cap - count * base;
    for node in &mut boundary {
        let size = remaining.min(256);
        node.display_label = Some("x".repeat(size));
        remaining -= size;
    }
    assert_eq!(remaining, 0);
    assert_eq!(
        derived_graph_bytes(&boundary, &[], &[], source_bytes).unwrap(),
        cap as u64
    );
    let last = boundary
        .iter_mut()
        .find(|node| node.display_label.as_ref().unwrap().len() < 256)
        .unwrap();
    last.display_label.as_mut().unwrap().push('x');
    assert!(
        derived_graph_bytes(&boundary, &[], &[], source_bytes)
            .unwrap_err()
            .to_string()
            .contains("graph row byte budget exceeded")
    );
    // A tiny test limit exercises the same SQL size gate without a 256 MiB fixture.
    assert!(
        Store::selected_source_row_bounded(&db, "one.js", 4)
            .unwrap_err()
            .to_string()
            .contains("byte budget exceeded")
    );
    db.execute(
            "UPDATE document_versions SET source_bytes=?1,byte_length=length(?1) WHERE vid=(SELECT vid FROM revision_documents WHERE path='one.js')",
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
            "UPDATE document_versions SET source_bytes=x'ff',byte_length=1 WHERE vid=(SELECT vid FROM revision_documents WHERE path='one.js')",
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

/// The extraction gate must never admit what the selected read refuses, so its charge can't
/// fall below the read's, and it stays close enough not to gap a file the read would admit.
#[test]
fn the_extraction_charge_covers_what_a_selected_read_charges() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for (path, source) in [
        (
            "dense.js",
            "import { a } from './b.js';\nfunction f(x, y) { if (x) { a.b(y); } else { g(); } }\n",
        ),
        (
            "dense.py",
            "import a.b\nfrom c import d\ndef f(x: int, y: str) -> int:\n    if x:\n        a.b(y)\n    return 1\n",
        ),
        // NFKC makes this callee's stored key several times its spelling.
        ("expanding.py", "\u{FDF2}()\n\u{FDF2}()\n\u{FDF2}()\n"),
        (
            "dense.rs",
            "use a::b;\nfn f<T>(x: u32, y: T) -> u32 { fn g(z: u32) { if z > 1 { h(); } } g(x); vec![x].len(); other!(); 1 }\n",
        ),
    ] {
        fs::write(root.path().join(path), source).unwrap();
    }
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
    let selected = store.read_revision_for(&db, None, true).unwrap();
    for document in &native.revision.documents {
        let key = &document.key;
        let (vid, cid, version): (i64, i64, String) = db
            .query_row(
                "SELECT m.vid,m.cid,d.id FROM revision_documents m JOIN document_versions d ON d.vid=m.vid
                 WHERE m.revision_id=?1 AND m.path=?2",
                params![selected.key, key.path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        let (_, stored, _) =
            Store::selected_ancillary_byte_usage(&db, vid, cid, &version, &key.source_set_id)
                .unwrap();
        let declarations: Vec<_> = native
            .declarations
            .iter()
            .filter(|row| row.document == *key)
            .cloned()
            .collect();
        let calls: Vec<_> = native
            .calls
            .iter()
            .filter(|row| row.document == *key)
            .cloned()
            .collect();
        let control_regions: Vec<_> = native
            .control_regions
            .iter()
            .filter(|row| row.document == *key)
            .cloned()
            .collect();
        let references: Vec<_> = native
            .references
            .iter()
            .filter(|row| row.document == *key)
            .cloned()
            .collect();
        let report = native
            .capability_reports
            .iter()
            .find(|report| report.document == *key)
            .unwrap();
        let charged = crate::evidence_budget::native_bytes(
            &crate::evidence_budget::Facts {
                declarations: &declarations,
                calls: &calls,
                control_regions: &control_regions,
                references: &references,
                gaps: &report.families[0].gaps,
                macro_expansions: report.macro_expansions.len(),
            },
            &key.source_set_id,
            capture
                .files
                .iter()
                .find(|file| file.path == key.path)
                .unwrap(),
        );
        let stored = stored as usize;
        assert!(version.len() <= 76, "{version}");
        assert!(charged >= stored, "{}: {charged} < {stored}", key.path);
        assert!(
            charged <= stored + stored / 5,
            "{}: {charged} > {stored}",
            key.path
        );
    }
}
