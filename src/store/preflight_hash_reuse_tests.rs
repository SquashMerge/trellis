//! R-1 (#199): publication reuse compares fresh hashes with the stored ones.
//! A stored mismatch is typed index corruption, and the next publication
//! rebaselines from fresh facts.
use super::*;
use crate::{index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
use std::{fs, sync::atomic::AtomicBool};

fn fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    IndexOptions,
    IndexPin,
) {
    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("A.java"),
        "class A { int run(){ return helper(); } int helper(){ return 1; } }\n",
    )
    .unwrap();
    fs::write(work.path().join("b.js"), "function b(){ return 2; }\n").unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let pin = IndexJobCoordinator::prepare(&store, None)
        .unwrap()
        .run(&options, &Arc::new(AtomicBool::new(false)), |_| {})
        .unwrap();
    (state, work, store, options, pin)
}

/// Overwrite one stored hash of A.java's selected rows with zeros.
fn corrupt(store: &Store, pin: IndexPin, sql: &str) {
    let mut db = store.cache_write().unwrap();
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .unwrap();
    let selected = store.read_revision_for(&tx, Some(pin), true).unwrap();
    assert!(tx.execute(sql, params![selected.key, "A.java"]).unwrap() > 0);
    tx.commit().unwrap();
}

/// A new file makes the next capture a FULL publication that reuses A.java.
fn publish_full(store: &Store, options: &IndexOptions) -> Result<IndexPin> {
    fs::write(
        options.workspace_root.join("c.js"),
        "function c(){ return 3; }\n",
    )
    .unwrap();
    IndexJobCoordinator::prepare(store, None)?.run(
        options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
}

fn stored_mismatch_fails_typed_then_rebaselines(sql: &str, message: &str) {
    let (_state, _work, store, options, pin) = fixture();
    corrupt(&store, pin, sql);
    let error = publish_full(&store, &options).expect_err("a stored mismatch must not publish");
    assert!(format!("{error:#}").contains(message), "{error:#}");
    assert!(
        format!("{error:#}").starts_with("incompatible_index"),
        "{error:#}"
    );
    assert!(store.disposition() == RecoveryDisposition::Rebuild);
    let head: (String, i64) = store
        .cache_write()
        .unwrap()
        .query_row(
            "SELECT index_generation,index_revision FROM index_metadata",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        head,
        (pin.index_generation.to_string(), pin.index_revision as i64),
        "no partial publication"
    );
    // Fallback: the next publication rebaselines from freshly measured facts.
    let rebuilt = publish_full(&store, &options).unwrap();
    assert_ne!(rebuilt.index_generation, pin.index_generation);
    assert!(store.disposition() == RecoveryDisposition::Ready);
    let graph = store.graph_at(Some(rebuilt)).unwrap();
    assert_eq!(graph.files.len(), 3);
    assert!(graph.nodes.iter().any(|node| node.name == "helper"));
    // The rebaselined head reuses cleanly: one more full publication works.
    fs::write(
        options.workspace_root.join("d.js"),
        "function d(){ return 4; }\n",
    )
    .unwrap();
    let next = IndexJobCoordinator::prepare(&store, None)
        .unwrap()
        .run(&options, &Arc::new(AtomicBool::new(false)), |_| {})
        .unwrap();
    assert_eq!(next.index_generation, rebuilt.index_generation);
}

#[test]
fn a_stored_native_witness_mismatch_fails_typed_then_rebaselines() {
    stored_mismatch_fails_typed_then_rebaselines(
        "UPDATE document_versions SET native_witness=printf('%064d',0)
         WHERE id=(SELECT document_version_id FROM revision_documents
                   WHERE revision_id=?1 AND path=?2)",
        "reusable normalized native witness mismatch",
    );
}

#[test]
fn a_stored_graph_hash_mismatch_fails_typed_then_rebaselines() {
    stored_mismatch_fails_typed_then_rebaselines(
        "UPDATE graph_projections SET graph_hash=printf('%064d',0)
         WHERE id=(SELECT graph_projection_id FROM revision_documents
                   WHERE revision_id=?1 AND path=?2)",
        "reusable graph witness mismatch",
    );
}

#[test]
fn a_stored_class_hash_mismatch_fails_typed_then_rebaselines() {
    stored_mismatch_fails_typed_then_rebaselines(
        "UPDATE class_projections SET content_hash=printf('%064d',0)
         WHERE id=(SELECT class_projection_id FROM revision_documents
                   WHERE revision_id=?1 AND path=?2)",
        "reusable class witness mismatch",
    );
}

/// Equal stored hashes are reused without decoding a stored row: a payload
/// the reuse no longer reads cannot fail the publication.
#[test]
fn equal_stored_hashes_reuse_without_decoding_stored_rows() {
    let (_state, _work, store, options, pin) = fixture();
    corrupt(
        &store,
        pin,
        "UPDATE graph_nodes SET payload='{}'
         WHERE projection_id=(SELECT graph_projection_id FROM revision_documents
                              WHERE revision_id=?1 AND path=?2)",
    );
    let next = publish_full(&store, &options).unwrap();
    assert_eq!(next.index_generation, pin.index_generation);
    assert_eq!(next.index_revision, pin.index_revision + 1);
}

#[test]
fn scratch_preflight_faults_do_not_reclassify_the_live_head() {
    for fault in ["graph_json", "extraction_json", "sqlite_corrupt"] {
        let (_state, _work, store, options, pin) = fixture();
        *store.test_stream_before_preflight_hook.lock().unwrap() = Some(Box::new(move |spool| {
            let index = spool
                .header()
                .revision
                .documents
                .iter()
                .position(|document| document.key.path == "A.java")
                .unwrap();
            match fault {
                "graph_json" => spool.corrupt_graph_json_for_tests(index),
                "extraction_json" => spool.corrupt_extraction_json_for_tests(index),
                "sqlite_corrupt" => spool.inject_sqlite_corrupt_read_for_tests(),
                _ => unreachable!(),
            }
        }));
        let error = publish_full(&store, &options).unwrap_err();
        assert!(
            error.downcast_ref::<NativeSpoolReadFailure>().is_some(),
            "{fault}: {error:#}"
        );
        assert_eq!(store.disposition(), RecoveryDisposition::Ready, "{fault}");
        let db = store.cache_write().unwrap();
        let selected: (String, i64) = db
            .query_row(
                "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            selected,
            (pin.index_generation.to_string(), pin.index_revision as i64),
            "{fault}: a scratch failure cannot partially publish"
        );
        let revisions: i64 = db
            .query_row("SELECT count(*) FROM native_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(revisions, 1, "{fault}");
        drop(db);
        let next = publish_full(&store, &options).unwrap();
        assert_eq!(next.index_generation, pin.index_generation, "{fault}");
        assert_eq!(next.index_revision, pin.index_revision + 1, "{fault}");
    }
}
