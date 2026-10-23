//! R-1 (#199): reused projections compare fresh hashes with stored witnesses.
//! Non-reused class hashes are replaced by fresh, validated projections; a
//! retained selected read still attests its own class rows.
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

fn call_lookups(store: &Store) -> Vec<(String, Vec<u8>, Option<String>)> {
    let db = store.cache_write().unwrap();
    let mut statement = db
        .prepare("SELECT v.id,c.id,c.callee_lookup_key FROM native_version_calls c JOIN document_versions v ON v.vid=c.vid ORDER BY v.id,c.id")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn streamed_callee_lookups_equal_plain_without_source_blob_reads() {
    let before = writer_source_sql_reads_for_tests();
    let (_state, work, streamed, options, _pin) = fixture();
    let stream_lookups = call_lookups(&streamed);
    assert!(
        !stream_lookups.is_empty(),
        "fixture must contain a native call"
    );
    assert_eq!(writer_source_sql_reads_for_tests() - before, 0);

    let state = tempfile::tempdir().unwrap();
    let plain = Store::open_for_tests(state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) =
        crate::indexer::index_workspace_bundle(&options, plain.root_id(), &cancel, |_| {}).unwrap();
    let baseline = plain.index_baseline().unwrap();
    let leader = plain.writer().unwrap();
    plain
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    assert_eq!(call_lookups(&plain), stream_lookups);
    assert_eq!(
        writer_source_sql_reads_for_tests() - before,
        native.calls.len(),
        "plain artifact still checks stored source bytes for each call"
    );
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
         WHERE vid=(SELECT vid FROM revision_documents
                   WHERE revision_id=?1 AND path=?2)",
        "reusable normalized native witness mismatch",
    );
}

#[test]
fn a_stored_graph_hash_mismatch_fails_typed_then_rebaselines() {
    stored_mismatch_fails_typed_then_rebaselines(
        "UPDATE graph_projections SET graph_hash=printf('%064d',0)
         WHERE gid=(SELECT gid FROM revision_documents
                   WHERE revision_id=?1 AND path=?2)",
        "class reused with different graph projection",
    );
}

#[test]
fn corrupted_non_reused_class_hash_reprojects_r2_but_retained_r1_fails_typed() {
    let (_state, _work, store, options, r1) = fixture();
    let expected = store
        .classes_at(Some("A.java"), "", Some(r1), 0, 100)
        .unwrap();
    assert!(!expected.items.is_empty(), "fixture must contain classes");
    corrupt(
        &store,
        r1,
        "UPDATE class_projections SET content_hash=printf('%064d',0)
         WHERE cid=(SELECT cid FROM revision_documents
                   WHERE revision_id=?1 AND path=?2)",
    );

    let r2 = publish_full(&store, &options).unwrap();
    assert_eq!(r2.index_generation, r1.index_generation);
    assert_eq!(r2.index_revision, r1.index_revision + 1);
    let db = store.cache_write().unwrap();
    let selected_r1 = store.read_revision_for(&db, Some(r1), true).unwrap();
    let selected_r2 = store.read_revision_for(&db, Some(r2), true).unwrap();
    let cid = |revision| {
        db.query_row(
            "SELECT cid FROM revision_documents WHERE revision_id=?1 AND path='A.java'",
            [revision],
            |row| row.get::<_, i64>(0),
        )
        .unwrap()
    };
    assert_ne!(
        cid(selected_r1.key),
        cid(selected_r2.key),
        "r2 writes a fresh class projection"
    );
    drop(db);
    let fresh = store
        .classes_at(Some("A.java"), "", Some(r2), 0, 100)
        .unwrap();
    assert_eq!(fresh.revision, r2);
    assert_eq!(
        serde_json::to_value(&fresh.items).unwrap(),
        serde_json::to_value(&expected.items).unwrap()
    );

    // The selected read returns only an error: no partial class page can escape.
    let error = store
        .classes_at(Some("A.java"), "", Some(r1), 0, 100)
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "incompatible_index: selected evidence decode failed: incompatible_index: selected projection fingerprint mismatch"
    );

    // A retained r1 read is valid after r2 publication when r1 was not corrupt.
    let (_clean_state, _clean_work, clean_store, clean_options, clean_r1) = fixture();
    let clean_expected = clean_store
        .classes_at(Some("A.java"), "", Some(clean_r1), 0, 100)
        .unwrap();
    let clean_r2 = publish_full(&clean_store, &clean_options).unwrap();
    assert_eq!(clean_r2.index_generation, clean_r1.index_generation);
    assert_eq!(clean_r2.index_revision, clean_r1.index_revision + 1);
    let retained = clean_store
        .classes_at(Some("A.java"), "", Some(clean_r1), 0, 100)
        .unwrap();
    assert_eq!(retained.revision, clean_r1);
    assert_eq!(
        serde_json::to_value(&retained.items).unwrap(),
        serde_json::to_value(&clean_expected.items).unwrap()
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
        "UPDATE graph_nodes SET name=name||'not-a-matching-name'
         WHERE gid=(SELECT gid FROM revision_documents
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

#[test]
fn first_publication_scratch_reads_fail_typed_without_partial_head() {
    for fault in ["graph_json", "extraction_json"] {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("A.java"), "class A { void f(){} }\n").unwrap();
        fs::write(work.path().join("b.js"), "function b(){ return 1; }\n").unwrap();
        let options = IndexOptions::new(work.path().to_owned());
        let store = Store::open_for_tests(state.path(), work.path()).unwrap();
        let baseline = store.index_baseline().unwrap();
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
                _ => unreachable!(),
            }
        }));
        let error = IndexJobCoordinator::prepare(&store, None)
            .unwrap()
            .run(&options, &Arc::new(AtomicBool::new(false)), |_| {})
            .unwrap_err();
        assert!(
            error.downcast_ref::<NativeSpoolReadFailure>().is_some(),
            "{fault}: {error:#}"
        );
        assert_eq!(store.index_baseline().unwrap(), baseline, "{fault}");
        let revisions: i64 = store
            .cache_write()
            .unwrap()
            .query_row("SELECT count(*) FROM native_revisions", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            revisions, 0,
            "{fault}: failed first publication left no revision"
        );
        let retry = IndexJobCoordinator::prepare(&store, None)
            .unwrap()
            .run(&options, &Arc::new(AtomicBool::new(false)), |_| {})
            .unwrap();
        assert_eq!(retry.index_revision, 1, "{fault}");
        assert_eq!(
            store.graph_at(Some(retry)).unwrap().files.len(),
            2,
            "{fault}"
        );
    }
}
