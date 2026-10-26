//! #225: equal document/graph/class IDs trust the no-drift validated head.
//! Drift and partial-family reuse keep R-1's witness checks.
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
fn streamed_callee_lookups_equal_plain_without_repeated_source_blob_reads() {
    let stream_state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("A.java"),
        "class A { int helper() { return 1; } int run() { return helper() + helper() + helper(); } }\n",
    )
    .unwrap();
    fs::write(
        work.path().join("b.js"),
        "function helper() { return 1; } function b() { return helper() + helper(); }\n",
    )
    .unwrap();
    fs::write(work.path().join("empty.rs"), "fn no_calls() {}\n").unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let streamed = Store::open_for_tests(stream_state.path(), work.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let before_stream = writer_source_sql_reads_for_tests();
    IndexJobCoordinator::prepare(&streamed, None)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    let stream_lookups = call_lookups(&streamed);
    assert_eq!(writer_source_sql_reads_for_tests() - before_stream, 0);

    let state = tempfile::tempdir().unwrap();
    let plain = Store::open_for_tests(state.path(), work.path()).unwrap();
    let (graph, native, capture) =
        crate::indexer::index_workspace_bundle(&options, plain.root_id(), &cancel, |_| {}).unwrap();
    let mut calls_by_path = BTreeMap::new();
    for call in &native.calls {
        *calls_by_path
            .entry(call.document.path.as_str())
            .or_insert(0) += 1;
    }
    assert_eq!(calls_by_path, BTreeMap::from([("A.java", 3), ("b.js", 2)]));
    assert!(graph.files.iter().any(|file| file.path == "empty.rs"));
    let baseline = plain.index_baseline().unwrap();
    let leader = plain.writer().unwrap();
    let before_plain = writer_source_sql_reads_for_tests();
    let pin = plain
        .publish_native(&graph, &capture, &native, &leader, baseline, &cancel)
        .unwrap();
    assert_eq!(call_lookups(&plain), stream_lookups);
    assert_eq!(
        writer_source_sql_reads_for_tests() - before_plain,
        2,
        "plain artifact reads each call-bearing document once"
    );

    let before_reuse = writer_source_sql_reads_for_tests();
    plain
        .publish_native(&graph, &capture, &native, &leader, pin, &cancel)
        .unwrap();
    assert_eq!(writer_source_sql_reads_for_tests() - before_reuse, 0);
    assert_eq!(call_lookups(&plain), stream_lookups);
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

/// Force the production full stream seam, independently of local eligibility.
fn publish_full(store: &Store, options: &IndexOptions) -> Result<IndexPin> {
    let pin = store
        .publication_index_baseline()?
        .pin()
        .context("selected pin missing")?;
    publish_full_route(store, options, pin, true)
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
fn native_v9_rows_survive_both_full_routes_and_local_reuse() {
    for stream in [false, true] {
        let state = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        fs::write(
            work.path().join("flow.rs"),
            "fn main() { run(); opaque!(hidden); }\n",
        )
        .unwrap();
        fs::write(work.path().join("b.js"), "function b(){ return 1; }\n").unwrap();
        let options = IndexOptions::new(work.path().to_owned());
        let store = Store::open_for_tests(state.path(), work.path()).unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let first = IndexJobCoordinator::prepare(&store, None)
            .unwrap()
            .run(&options, &cancel, |_| {})
            .unwrap();
        let key = crate::native_evidence::DocumentKey {
            source_set_id: format!("source-set:v1:{}", store.root_id()),
            language: "rust".into(),
            path: "flow.rs".into(),
        };
        let snapshot = |pin| {
            store.native_at(pin, |db, selected| {
                let scope = v8_native_scope_for(db, &key, selected)?.unwrap();
                let ids: (i64, i64, i64) = db.query_row(
                    "SELECT vid,gid,cid FROM revision_documents WHERE revision_id=?1 AND path='flow.rs'",
                    [&selected.key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
                let mut rows = vec![];
                for table in [
                    "native_version_references",
                    "native_version_capability_families",
                    "native_version_capability_gaps",
                ] {
                    let mut statement = db.prepare(&format!("SELECT * FROM {table} WHERE vid=?1 ORDER BY 1,2,3"))?;
                    let columns = statement.column_count();
                    let values = statement.query_map([scope.vid], |row| {
                        (0..columns).map(|column| row.get::<_, rusqlite::types::Value>(column))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })?.collect::<rusqlite::Result<Vec<_>>>()?;
                    assert!(!values.is_empty(), "fixture must exercise {table}");
                    rows.push(values);
                }
                Ok((ids, rows, read_native_references(db, &scope, None)?,
                    read_native_capability_report(db, &scope)?,
                    read_native_coverage_v8(db, &scope)?))
            }).unwrap()
        };
        let original = snapshot(first);
        assert_eq!(original.4.state, "partial");
        assert!(!original.4.observed_roles.is_empty());
        let full = publish_full_route(&store, &options, first, stream).unwrap();
        assert_eq!(
            store
                .last_writer_counters()
                .unwrap()
                .reused_occurrence_reads,
            0
        );
        fs::write(
            work.path().join("b.js"),
            "function changed(){ return 2; }\n",
        )
        .unwrap();
        let local = AtomicBool::new(false);
        let current = IndexJobCoordinator::prepare(&store, Some(full))
            .unwrap()
            .run(&options, &cancel, |progress| {
                if progress.phase == "mode:local" {
                    local.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            })
            .unwrap();
        assert!(local.load(std::sync::atomic::Ordering::Relaxed));
        assert_eq!(
            store
                .last_writer_counters()
                .unwrap()
                .reused_occurrence_reads,
            0
        );
        for pin in [full, current] {
            let (ids, rows, mut references, mut report, mut coverage) = snapshot(pin);
            assert_eq!(ids, original.0);
            assert_eq!(rows, original.1);
            assert_ne!(report.revision_id, original.3.revision_id);
            for reference in &mut references {
                assert_eq!(reference.revision_id, report.revision_id);
                reference.revision_id.clone_from(&original.3.revision_id);
                reference
                    .provenance_id
                    .clone_from(&original.2[0].provenance_id);
            }
            report.revision_id.clone_from(&original.3.revision_id);
            coverage.revision_id.clone_from(&original.3.revision_id);
            assert_eq!(references, original.2);
            assert_eq!(report, original.3);
            assert_eq!(coverage, original.4);
        }
        assert_eq!(snapshot(first), original, "retained pin must not change");
        assert_eq!(store.status().unwrap().stats.parse_error_files, 0);
    }
}

#[test]
fn no_drift_matching_ids_trust_stored_native_and_extraction_witnesses() {
    for stream in [false, true] {
        let (_state, _work, store, options, pin) = fixture();
        corrupt(
            &store,
            pin,
            "UPDATE document_versions SET native_witness=printf('%064d',0)
             WHERE vid=(SELECT vid FROM revision_documents
                       WHERE revision_id=?1 AND path=?2)",
        );
        corrupt(
            &store,
            pin,
            "UPDATE graph_projections SET class_extraction_payload='{}'
             WHERE gid=(SELECT gid FROM revision_documents
                       WHERE revision_id=?1 AND path=?2)",
        );
        let next = publish_full_route(&store, &options, pin, stream).unwrap();
        assert_eq!(next.index_generation, pin.index_generation);
        assert_eq!(next.index_revision, pin.index_revision + 1);
    }
}

fn publish_full_route(
    store: &Store,
    options: &IndexOptions,
    pin: IndexPin,
    stream: bool,
) -> Result<IndexPin> {
    fs::write(
        options.workspace_root.join("c.js"),
        "function c(){ return 3; }\n",
    )?;
    let cancel = Arc::new(AtomicBool::new(false));
    let leader = store.writer()?;
    if stream {
        let capture = crate::capture::Capture::admit(options, &cancel, &|_| {})?;
        let spool = crate::native_evidence::spool_full_capture(
            options,
            &capture,
            &options.workspace_root,
            store.root_id(),
            &cancel,
            &|_| {},
            |_, _| {},
            &store.native_spool_dir()?,
        )?;
        store.publish_validated_stream_recovery(
            &spool,
            &capture,
            &leader,
            store.publication_index_baseline()?,
            &cancel,
        )
    } else {
        let (graph, native, capture) =
            crate::indexer::index_workspace_bundle(options, store.root_id(), &cancel, |_| {})?;
        store.publish_native(&graph, &capture, &native, &leader, pin, &cancel)
    }
}

#[test]
fn producer_drift_keeps_native_witness_comparison_for_both_full_routes() {
    for stream in [false, true] {
        let (_state, _work, store, options, pin) = fixture();
        // Model a validated prior publication from a different executable.
        let db = store.cache_write().unwrap();
        let selected = store.read_revision_for(&db, Some(pin), true).unwrap();
        assert_eq!(db.execute(
            "UPDATE revision_capture_inputs SET payload=json_set(payload,'$.hash',printf('%064d',0))
             WHERE revision_id=?1 AND input_key LIKE 'executable:%'",
            [&selected.key],
        ).unwrap(), 1);
        let (producer, binding) = revision_producer_binding(&db, &selected.key).unwrap();
        assert_eq!(db.execute(
            "UPDATE revision_producer_bindings SET producer_sha=?2,binding_sha=?3 WHERE revision_id=?1",
            params![selected.key, producer, binding],
        ).unwrap(), 1);
        drop(db);
        corrupt(
            &store,
            pin,
            "UPDATE document_versions SET native_witness=printf('%064d',0)
             WHERE vid=(SELECT vid FROM revision_documents
                       WHERE revision_id=?1 AND path=?2)",
        );
        let error = publish_full_route(&store, &options, pin, stream).unwrap_err();
        assert!(
            format!("{error:#}").contains("native_producer_version_required"),
            "{error:#}"
        );
        assert_eq!(store.index_baseline().unwrap(), pin);
    }
}

#[test]
fn partial_family_reuse_keeps_native_witness_comparison_for_both_full_routes() {
    for stream in [false, true] {
        let (_state, _work, store, options, pin) = fixture();
        corrupt(
            &store,
            pin,
            "UPDATE document_versions SET native_witness=printf('%064d',0)
             WHERE vid=(SELECT vid FROM revision_documents WHERE revision_id=?1 AND path=?2)",
        );
        corrupt(
            &store,
            pin,
            "UPDATE class_projections SET content_hash=printf('%064d',0)
             WHERE cid=(SELECT cid FROM revision_documents WHERE revision_id=?1 AND path=?2)",
        );
        let error = publish_full_route(&store, &options, pin, stream).unwrap_err();
        assert!(
            format!("{error:#}").contains("reusable normalized native witness mismatch"),
            "{error:#}"
        );
        assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
    }
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
fn corrupted_non_reused_class_hash_reprojects_r2() {
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

    // Reads trust the published index (#261): a well-formed but wrong stored
    // class hash is not recomputed on read, so a retained r1 still serves.
    assert_eq!(
        store
            .classes_at(Some("A.java"), "", Some(r1), 0, 100)
            .unwrap()
            .revision,
        r1
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
