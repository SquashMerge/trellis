use super::*;

#[test]
fn selected_capture_and_reconcile_use_the_revision_index() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V8).unwrap();
    for (label, sql) in [
        ("capture", SELECTED_CAPTURE_SNAPSHOT_SQL),
        ("reconcile", SELECTED_RECONCILE_FILES_SQL),
    ] {
        let plan: Vec<String> = db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |row| row.get(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter()
                .any(|step| step.starts_with("SEARCH d USING INDEX ")
                    && step.contains("(revision_id=?)")),
            "{label} must use the selected manifest index: {plan:?}"
        );
        assert!(
            plan.iter().any(|step| step.starts_with("SCALAR SUBQUERY")),
            "{label} must be anchored to singleton-selected metadata: {plan:?}"
        );
        assert!(
            !plan.iter().any(|step| step.starts_with("SCAN d")),
            "{label} scanned historical manifests: {plan:?}"
        );
    }
}

#[test]
fn incomplete_current_schema_is_rejected() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V8).unwrap();
    db.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)
        .unwrap();
    validate_cache_shape(&db).unwrap();
    db.execute_batch("DROP INDEX native_revisions_header_cover")
        .unwrap();
    let failure = validate_cache_shape(&db).unwrap_err();
    assert!(
        failure
            .to_string()
            .contains("incompatible_index: unknown cache object type, name or shape"),
        "{failure:#}"
    );
}

#[test]
fn per_file_manifest_lookup_uses_the_path_index() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V8).unwrap();
    db.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)
        .unwrap();
    validate_cache_shape(&db).unwrap();
    for sql in [
        "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?2 AND path=?1)",
        "SELECT d.source_bytes FROM revision_documents m JOIN document_versions d
         ON d.id=m.document_version_id WHERE m.revision_id=?2 AND m.path=?1",
    ] {
        let plan: Vec<String> = db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(["a.rs", "revision"], |row| row.get(3))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter()
                .any(|step| step.contains("revision_documents_path")
                    && step.contains("(revision_id=? AND path=?)")),
            "per-file lookup must not scan the revision's manifest: {plan:?}"
        );
    }
}

#[test]
fn header_and_changing_manifest_plan_avoid_wide_revision_scans() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V8).unwrap();
    db.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)
        .unwrap();
    validate_cache_shape(&db).unwrap();
    let header: Vec<String> = db
        .prepare(&format!("EXPLAIN QUERY PLAN {HEADER_SQL}"))
        .unwrap()
        .query_map(["source-set:v1:test"], |row| row.get(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        header
            .iter()
            .any(|step| step.contains("SCAN r USING COVERING INDEX native_revisions_header_cover")),
        "header check must not load retained JSON: {header:?}"
    );
    let changing = paired_manifest_query(PairedManifestScope::Changing);
    let plan: Vec<String> = db
        .prepare(&format!("EXPLAIN QUERY PLAN {changing}"))
        .unwrap()
        .query_map([], |row| row.get(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(
        plan.iter()
            .any(|step| step.starts_with("SEARCH r USING INDEX ") && step.contains("(id=?)")),
        "changing manifest must lookup each revision key: {plan:?}"
    );
    assert!(
        !plan.iter().any(|step| step.starts_with("SCAN r")),
        "changing manifest scanned all revision headers: {plan:?}"
    );
}

#[test]
fn first_publication_creates_epoch_and_attests_selected_producer() {
    use crate::index_coordinator::IndexJobCoordinator;
    use crate::indexer::IndexOptions;
    use std::{fs, sync::atomic::AtomicBool};

    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(work.path().join("A.java"), "class A { int run() { return 1; } }\n")
        .unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let pin = IndexJobCoordinator::prepare(&store, None)
        .unwrap()
        .run(
            &IndexOptions::new(work.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
    let db = store.cache_write().unwrap();
    let epoch: (String, i64) = db
        .query_row(
            "SELECT index_generation,first_revision FROM native_binding_epoch WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(epoch, (pin.index_generation.to_string(), 1));
    let selected = ReadRevision::current(&db).unwrap();
    assert_eq!(selected.pin, pin);
    let derived = selected_producer_hash(&db, &selected).unwrap();
    let persisted: String = db
        .query_row(
            "SELECT producer_sha FROM revision_producer_bindings WHERE revision_id=?1",
            [&selected.key],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(derived, persisted);
}

#[test]
fn writer_session_serves_valid_prior_head_through_reconciliation() {
    use crate::index_coordinator::IndexJobCoordinator;
    use crate::indexer::IndexOptions;
    use std::{fs, sync::atomic::AtomicBool};

    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    fs::write(
        work.path().join("A.java"),
        "class A { int run() { return 1; } }\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let pin = job
        .run(
            &IndexOptions::new(work.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
    drop(store);
    let reopened = Store::open_for_tests(state.path(), work.path()).unwrap();
    let new_session = reopened.writer_session().unwrap();
    let selected = reopened.evidence_response_daemon_local().unwrap();
    let before_h = selected.status().unwrap();
    let pinned = selected.source_at("A.java", Some(pin)).unwrap().unwrap();
    let unpinned = selected.source_at("A.java", None).unwrap().unwrap();
    selected.finish(()).unwrap();
    drop(selected);
    assert!(
        reopened
            .verify_committed_writer_claim(&new_session)
            .is_err(),
        "a new local writer must reconcile before claiming requests"
    );
    assert_eq!(before_h.revision, pin);
    assert_eq!(pinned.0, pin);
    assert_eq!(unpinned.0, pin);

    fs::write(
        work.path().join("A.java"),
        "class A { int run() { return 2; } }\n",
    )
    .unwrap();
    let abort = Arc::new(AtomicBool::new(false));
    let signal = abort.clone();
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    reopened.set_publication_before_commit_hook_for_tests(move || {
        arrived_tx.send(()).unwrap();
        release_rx.recv().unwrap();
        signal.store(true, Ordering::Release);
    });
    let h = IndexJobCoordinator::prepare_with_session(&reopened, Some(pin), new_session.clone())
        .unwrap();
    let options = IndexOptions::new(work.path().to_owned());
    let worker = std::thread::spawn(move || h.run(&options, &abort, |_| {}));
    arrived_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .expect("reconciliation did not reach the writer-transaction barrier");
    let selected = reopened.evidence_response_daemon_local().unwrap();
    let during = selected.status().unwrap();
    let pinned_during = selected.source_at("A.java", Some(pin)).unwrap().unwrap();
    let unpinned_during = selected.source_at("A.java", None).unwrap().unwrap();
    selected.finish(()).unwrap();
    drop(selected);
    let premature_claim = reopened.verify_committed_writer_claim(&new_session);
    release_tx.send(()).unwrap();
    let failed = worker.join().unwrap().unwrap_err();
    assert!(failed.to_string().contains("cancelled"), "{failed:#}");
    assert_eq!(during.revision, pin);
    assert_eq!(pinned_during.0, pin);
    assert_eq!(unpinned_during.0, pin);
    assert!(
        premature_claim.is_err(),
        "a writer cannot claim before COMMIT"
    );
    let selected = reopened.evidence_response_daemon_local().unwrap();
    assert_eq!(selected.status().unwrap().revision, pin);
    assert_eq!(selected.source_at("A.java", None).unwrap().unwrap().0, pin);
    selected.finish(()).unwrap();
    drop(selected);
    assert!(
        reopened
            .verify_committed_writer_claim(&new_session)
            .is_err()
    );

    // A successful retry selects the new head and enables the local writer.
    let next = IndexJobCoordinator::prepare_with_session(&reopened, Some(pin), new_session.clone())
        .unwrap()
        .run(
            &IndexOptions::new(work.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
    assert!(next.index_revision > pin.index_revision);
    reopened
        .verify_committed_writer_claim(&new_session)
        .unwrap();
    assert_eq!(reopened.status().unwrap().revision, next);
    assert_eq!(
        reopened.source_at("A.java", Some(pin)).unwrap().unwrap().0,
        pin
    );
    assert_eq!(
        reopened.source_at("A.java", None).unwrap().unwrap().1.text,
        "class A { int run() { return 2; } }\n"
    );
}

#[test]
fn routine_publication_keeps_prior_pinned_and_unpinned_reads_available() {
    use crate::index_coordinator::IndexJobCoordinator;
    use crate::indexer::IndexOptions;
    use std::{fs, sync::atomic::AtomicBool};

    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("A.java");
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    fs::write(&source, "class A { int run() { return 1; } }\n").unwrap();
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = first_job.session();
    let first = first_job.run(&options, &cancel, |_| {}).unwrap();
    fs::write(&source, "class A { int run() { return 2; } }\n").unwrap();
    let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    store.set_publication_before_commit_hook_for_tests(move || {
        arrived_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    let job =
        IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone()).unwrap();
    let publisher = std::thread::spawn(move || job.run(&options, &cancel, |_| {}));
    arrived_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .expect("publication did not reach the writer-transaction barrier");
    let during_status = store.status();
    let during_pinned = store.source_at("A.java", Some(first));
    let during_unpinned = store.source_at("A.java", None);
    release_tx.send(()).unwrap();
    let next = publisher.join().unwrap().unwrap();
    assert_eq!(next.index_revision, first.index_revision + 1);
    assert_eq!(during_status.unwrap().revision, first);
    assert_eq!(during_pinned.unwrap().unwrap().0, first);
    assert_eq!(during_unpinned.unwrap().unwrap().0, first);
    assert_eq!(store.status().unwrap().revision, next);
    assert_eq!(
        store.source_at("A.java", Some(first)).unwrap().unwrap().0,
        first
    );
    // A failed publication must leave the last committed head available.
    fs::write(&source, "class A { int run() { return 3; } }\n").unwrap();
    let abort = Arc::new(AtomicBool::new(false));
    let signal = abort.clone();
    store.set_publication_before_commit_hook_for_tests(move || {
        signal.store(true, Ordering::Release);
    });
    let failure = IndexJobCoordinator::prepare_with_session(&store, Some(next), session.clone())
        .unwrap()
        .run(&IndexOptions::new(work.path().to_owned()), &abort, |_| {})
        .unwrap_err();
    assert!(failure.to_string().contains("cancelled"), "{failure:#}");
    assert_eq!(store.status().unwrap().revision, next);
    assert_eq!(store.source_at("A.java", None).unwrap().unwrap().0, next);
    assert_eq!(
        store.source_at("A.java", Some(first)).unwrap().unwrap().0,
        first
    );
}

#[test]
fn hot_pair_check_skips_stable_history_but_admission_checks_it() {
    use crate::index_coordinator::IndexJobCoordinator;
    use crate::indexer::IndexOptions;
    use std::{fs, sync::atomic::AtomicBool};

    let state = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    let source = work.path().join("A.java");
    let options = IndexOptions::new(work.path().to_owned());
    let cancel = Arc::new(AtomicBool::new(false));
    let store = Store::open_for_tests(state.path(), work.path()).unwrap();
    fs::write(&source, "class A { int run() { return 1; } }\n").unwrap();
    let first_job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let session = first_job.session();
    PAIRED_CHECK_COUNT.with(|count| count.set(0));
    let first = first_job.run(&options, &cancel, |_| {}).unwrap();
    let first_checks = PAIRED_CHECK_COUNT.with(|count| count.get());
    assert_eq!(
        first_checks, 2,
        "first publication validates selected and staged heads"
    );
    fs::write(&source, "class A { int run() { return 2; } }\n").unwrap();
    let second = IndexJobCoordinator::prepare_with_session(&store, Some(first), session.clone())
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    // Each publication validates its selected prior head on the read snapshot
    // and the staged changing head in the writer transaction.
    assert_eq!(
        PAIRED_CHECK_COUNT.with(|count| count.get()),
        first_checks + 2
    );
    fs::write(&source, "class A { int run() { return 3; } }\n").unwrap();
    let third = IndexJobCoordinator::prepare_with_session(&store, Some(second), session)
        .unwrap()
        .run(&options, &cancel, |_| {})
        .unwrap();
    assert_eq!(
        PAIRED_CHECK_COUNT.with(|count| count.get()),
        first_checks + 4
    );
    assert_eq!(third.index_revision, first.index_revision + 2);
    let first_key = format!("pin:v1:{}:{}", first.index_generation, first.index_revision);
    let third_key = format!("pin:v1:{}:{}", third.index_generation, third.index_revision);
    let db = store.cache_write().unwrap();
    let original: String = db
        .query_row(
            "SELECT source_inventory FROM native_revisions WHERE id=?1",
            [&first_key],
            |row| row.get(0),
        )
        .unwrap();
    db.execute(
        "UPDATE native_revisions SET source_inventory='[]' WHERE id=?1",
        [&first_key],
    )
    .unwrap();
    validate_paired_metadata(&db, store.root_id(), PairedManifestScope::Changing).unwrap();
    assert!(
        validate_paired_metadata(&db, store.root_id(), PairedManifestScope::Full)
            .unwrap_err()
            .to_string()
            .contains("retained manifest/release mismatch")
    );
    db.execute(
        "UPDATE native_revisions SET source_inventory=?1 WHERE id=?2",
        rusqlite::params![original, first_key],
    )
    .unwrap();
    db.execute(
        "UPDATE native_revisions SET source_inventory='[]' WHERE id=?1",
        [&third_key],
    )
    .unwrap();
    assert!(
        validate_paired_metadata(&db, store.root_id(), PairedManifestScope::Changing)
            .unwrap_err()
            .to_string()
            .contains("retained manifest/release mismatch")
    );
}
