use super::*;
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{fs, sync::atomic::AtomicBool};

fn published_store(files: usize) -> (tempfile::TempDir, tempfile::TempDir, Store) {
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

#[test]
fn metrics_revision_computes_snapshot_values_once_and_reads_each_source_once() {
    let (_state, _root, store) = published_store(3);
    let response = store.evidence_response().unwrap();
    take_selected_read_counts();
    let payload = response
        .metrics_at(&MetricsOptions::default(), &mut || Ok(()))
        .unwrap();
    assert_eq!(payload.files.len(), 3);
    assert_eq!(
        take_selected_read_counts(),
        SelectedReadCounts {
            // The selected revision's producer binding is checked once per snapshot.
            producer_hashes: 1,
            // Each document is still attested, reading and hashing its source once.
            source_reads: 3,
            attestations: 3,
        }
    );
}

#[test]
fn files_page_attests_only_the_rows_it_returns() {
    let (_state, _root, store) = published_store(3);
    take_selected_read_counts();
    let page = store.files_at(None, 0, 2).unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert_eq!(page["nextOffset"], 2);
    assert_eq!(take_selected_read_counts().attestations, 2);
    let last = store.files_at(None, 2, 2).unwrap();
    assert_eq!(last["items"].as_array().unwrap().len(), 1);
    assert!(last["nextOffset"].is_null());
    assert_eq!(take_selected_read_counts().attestations, 1);
}
