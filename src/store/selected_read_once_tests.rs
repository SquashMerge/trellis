use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{fs, sync::atomic::AtomicBool};

pub(super) fn published_store(files: usize) -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    for n in 0..files {
        fs::write(
            root.path().join(format!("f{n}.js")),
            format!("function f{n}(x) {{ if (x) {{ return g(x); }} return {n}; }}\n"),
        )
        .unwrap();
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
    (state, root, store)
}

/// Counts for one whole request: response admission plus the read itself.
fn whole_request<T>(read: impl FnOnce() -> T) -> (T, SelectedReadCounts) {
    take_selected_read_counts();
    let value = read();
    (value, take_selected_read_counts())
}

#[test]
fn metrics_revision_computes_snapshot_values_once_and_reads_each_source_once() {
    let (_state, _root, store) = published_store(3);
    let (payload, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
            .unwrap()
    });
    assert_eq!(payload.files.len(), 3);
    assert_eq!(
        counts,
        SelectedReadCounts {
            // The selected revision's producer binding is checked once per
            // request, at admission, and shared by the metrics read.
            producer_hashes: 1,
            // Each document's stored rows are read once, with its source.
            source_reads: 3,
            document_reads: 3,
        }
    );
}

#[test]
fn files_page_reads_no_selected_documents() {
    let (_state, _root, store) = published_store(3);
    let (page, counts) = whole_request(|| store.files_at(None, 0, 2).unwrap());
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["nextOffset"], 2);
    assert_eq!((counts.producer_hashes, counts.document_reads), (1, 0));
    let (last, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .files_at(None, 2, 2)
            .unwrap()
    });
    assert_eq!(last["items"].as_array().unwrap().len(), 1);
    assert!(last["nextOffset"].is_null());
    assert_eq!((counts.producer_hashes, counts.document_reads), (1, 0));
}

#[test]
fn mcp_reads_check_the_producer_binding_once_per_request() {
    let (_state, _root, store) = published_store(3);
    let pin = store
        .evidence_response()
        .unwrap()
        .status()
        .unwrap()
        .revision;
    let (owner, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_find("f0", 20, Some(pin))
            .unwrap()
            .symbols
            .into_iter()
            .find(|symbol| symbol.declaration.name.as_deref() == Some("f0"))
            .unwrap()
    });
    let mut hashes = vec![("find", counts.producer_hashes)];
    let id = owner.declaration.syntax_id.clone();
    let (_, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_language_counts()
            .unwrap()
    });
    hashes.push(("language counts", counts.producer_hashes));
    let (_, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_source("f0.js", 1, 1, Some(pin), None)
            .unwrap()
            .unwrap()
    });
    hashes.push(("source", counts.producer_hashes));
    let (_, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_declaration(&id, Some(pin))
            .unwrap()
            .unwrap()
    });
    hashes.push(("declaration", counts.producer_hashes));
    let now = std::time::Instant::now();
    let (_, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_outgoing(
                &id,
                20,
                Some(pin),
                &|| now,
                now + std::time::Duration::from_secs(5),
            )
            .unwrap()
            .unwrap()
    });
    hashes.push(("outgoing", counts.producer_hashes));
    assert_eq!(
        hashes,
        [
            ("find", 1),
            ("language counts", 1),
            ("source", 1),
            ("declaration", 1),
            ("outgoing", 1)
        ]
    );
}

#[test]
fn files_page_reports_a_dangling_graph_projection() {
    let (_state, _root, store) = published_store(2);
    {
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        assert_eq!(
            db.execute(
                "UPDATE revision_documents SET gid=-1 WHERE path='f1.js'",
                []
            )
            .unwrap(),
            1
        );
        db.pragma_update(None, "foreign_keys", "ON").unwrap();
    }
    // Not a zero method count: the missing projection is corruption.
    let error = store.files_at(None, 0, 10).unwrap_err();
    assert_eq!(
        format!("{error:#}"),
        "incompatible_index: selected document projection absent"
    );
    assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
}

#[test]
fn incoming_reads_each_selected_source_once_for_repeated_callers() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("target.js"),
        "function Target() { Target(); }\nfunction Caller() { Target(); Target(); }\n",
    )
    .unwrap();
    fs::write(
        root.path().join("other.js"),
        "function Other() { Target(); Target(); }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().into()),
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
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let target = native
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("Target"))
        .unwrap();
    let (incoming, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_incoming_with_lines(&target.syntax_id, 20, Some(pin))
            .unwrap()
            .unwrap()
    });
    assert_eq!(incoming.calls.len(), 5);
    assert!(incoming.calls.iter().all(|call| {
        call.callee_text == "Target"
            && call.source.lines.is_some()
            && call.call.owner_syntax_id == call.caller.syntax_id
    }));
    assert_eq!(counts.source_reads, 2);
}

#[test]
fn rust_lookup_keys_reuse_the_checked_source_for_sid_answers() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("store.rs"), "struct Store; impl Store { fn save(&self) {} fn caller(&self) { self.save(); self.save(); } }\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(root.path().into()),
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
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    let target = native
        .declarations
        .iter()
        .find(|declaration| declaration.name.as_deref() == Some("save"))
        .unwrap();
    let (found, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_find_with_lines("save", 20, Some(pin))
            .unwrap()
    });
    assert_eq!(counts.source_reads, 1);
    assert_eq!(
        found.symbols[0].source.stable_lookup_key.as_deref(),
        Some("store.rs::Store::save")
    );
    let (declaration, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_declaration_with_lines(&target.syntax_id, Some(pin))
            .unwrap()
            .unwrap()
    });
    assert_eq!(counts.source_reads, 1);
    assert_eq!(
        declaration.source.stable_lookup_key.as_deref(),
        Some("store.rs::Store::save")
    );
    let (incoming, counts) = whole_request(|| {
        store
            .evidence_response()
            .unwrap()
            .mcp_incoming_with_lines(&target.syntax_id, 20, Some(pin))
            .unwrap()
            .unwrap()
    });
    assert_eq!(incoming.calls.len(), 2);
    assert_eq!(counts.source_reads, 1);
    assert_eq!(
        incoming.selected.source.stable_lookup_key.as_deref(),
        Some("store.rs::Store::save")
    );
}
