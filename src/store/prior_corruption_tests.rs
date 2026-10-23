use super::*;
use crate::{capture::Capture, index_coordinator::IndexJobCoordinator, indexer::IndexOptions};
use std::{fs, sync::atomic::AtomicBool};

fn fixture() -> (tempfile::TempDir, tempfile::TempDir, Store, IndexOptions) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("local.js"),
        "function local(){return 1;}\n",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let options = IndexOptions::new(root.path().to_owned());
    reindex(&store, &options);
    (state, root, store, options)
}

fn reindex(store: &Store, options: &IndexOptions) -> IndexPin {
    let cancel = Arc::new(AtomicBool::new(false));
    let pin = IndexJobCoordinator::prepare(store, None)
        .unwrap()
        .run(options, &cancel, |_| {})
        .unwrap();
    assert!(!store.recovery_required.load(Ordering::Acquire));
    assert_eq!(store.status().unwrap().revision, pin);
    pin
}

fn pending(store: &Store) {
    assert!(store.recovery_required.load(Ordering::Acquire));
    assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
}

#[test]
fn watcher_baselines_schedule_rebuild_for_a_stored_generation_that_will_not_decode() {
    for publication in [false, true] {
        let (_state, _root, store, options) = fixture();
        store
            .cache_write()
            .unwrap()
            .execute(
                "UPDATE index_metadata SET index_generation='invalid-generation' WHERE singleton=1",
                [],
            )
            .unwrap();
        let baseline = if publication {
            store.publication_index_baseline()
        } else {
            store.recovery_index_baseline()
        }
        .unwrap();
        assert!(!baseline.compatible);
        pending(&store);
        reindex(&store, &options);
        assert!(store.recovery_index_baseline().unwrap().compatible);
    }
}

#[test]
fn local_prior_state_decode_failure_schedules_rebuild_and_the_next_index_reads_cleanly() {
    let (_state, root, store, options) = fixture();
    let baseline = store.publication_index_baseline().unwrap();
    fs::write(
        root.path().join("local.js"),
        "function local(){return 2;}\n",
    )
    .unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let capture = Capture::admit(&options, &cancel, &|_| {}).unwrap();
    store
        .cache_write()
        .unwrap()
        .execute_batch(
            "PRAGMA ignore_check_constraints=ON;
         UPDATE index_metadata SET stats='not-json' WHERE singleton=1;",
        )
        .unwrap();
    assert!(
        store
            .prepare_local_revision(&capture, &baseline, &cancel)
            .is_err()
    );
    pending(&store);
    reindex(&store, &options);
    assert_eq!(
        store.source_at("local.js", None).unwrap().unwrap().1.text,
        "function local(){return 2;}\n"
    );
}

#[test]
fn pin_validation_marks_a_missing_current_native_head_and_rebuild_restores_it() {
    let (_state, _root, store, options) = fixture();
    let pin = store.status().unwrap().revision;
    store
        .cache_write()
        .unwrap()
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
         DELETE FROM native_revisions WHERE published_index_revision=(
             SELECT index_revision FROM index_metadata WHERE singleton=1);",
        )
        .unwrap();
    assert!(
        store
            .with_evidence(|db| store.read_revision(db, Some(pin)))
            .is_err()
    );
    pending(&store);
    let restored = reindex(&store, &options);
    assert!(
        store
            .with_evidence(|db| store.read_revision(db, Some(restored)))
            .is_ok()
    );
}

#[test]
fn retention_inspection_row_decode_failure_schedules_rebuild_and_then_succeeds() {
    let (_state, root, store, options) = fixture();
    fs::write(
        root.path().join("local.js"),
        "function local(){return 2;}\n",
    )
    .unwrap();
    reindex(&store, &options);
    let db = store.cache_write().unwrap();
    db.execute_batch(
        "PRAGMA ignore_check_constraints=ON;
        UPDATE native_revision_supersessions SET superseded_at=x'ff';",
    )
    .unwrap();
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM native_revision_supersessions",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    drop(db);
    let writer = store.writer().unwrap();
    let error = store
        .oldest_due_debt_age(&writer)
        .map_err(|error| store.report_index_failure(error))
        .unwrap_err();
    assert_eq!(recovery_class(&error), RecoveryClass::Rebuild, "{error:#}");
    pending(&store);
    drop(writer);
    reindex(&store, &options);
    assert!(store.oldest_due_debt_age(&store.writer().unwrap()).is_ok());
}

#[test]
fn startup_admission_marks_corrupt_reconcile_options_and_rebuild_restores_reads() {
    let (state, root, store, options) = fixture();
    store
        .cache_write()
        .unwrap()
        .execute(
            "UPDATE index_metadata SET reconcile_options='not-json' WHERE singleton=1",
            [],
        )
        .unwrap();
    drop(store);
    let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
    pending(&reopened);
    reindex(&reopened, &options);
    assert!(reopened.source_at("local.js", None).unwrap().is_some());
}

#[test]
fn caller_pin_conflicts_do_not_schedule_rebuild() {
    let (_state, _root, store, _options) = fixture();
    let current = store.status().unwrap().revision;
    let foreign = IndexPin {
        index_generation: uuid::Uuid::new_v4(),
        index_revision: current.index_revision,
    };
    let future = IndexPin {
        index_revision: current.index_revision + 1,
        ..current
    };
    for pin in [foreign, future] {
        assert!(
            store
                .with_evidence(|db| store.read_revision(db, Some(pin)))
                .is_err()
        );
        assert!(!store.recovery_required.load(Ordering::Acquire));
        assert_eq!(store.disposition(), RecoveryDisposition::Ready);
    }
}
