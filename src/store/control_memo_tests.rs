//! #1245 PR 3: bounded control validation is remembered per committed file state.
use super::*;
use std::sync::atomic::Ordering::Relaxed;

const CORRUPT_INVENTORY: &str =
    "UPDATE revision_documents SET capture_stat='{}' WHERE path='f0.js'";

fn validations(store: &Store) -> usize {
    store.published_admission.control_validations.load(Relaxed)
}

/// A published store whose memo is empty, so the next read validates.
fn fresh_store() -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let (state, root, store) = selected_read_once_tests::published_store(2);
    store.published_admission.invalidate();
    (state, root, store)
}

/// A read-only snapshot of the live index and the file its connection opened.
fn read_snapshot(
    store: &Store,
    validate: impl FnOnce(&Connection, (u64, u64)) -> Result<()>,
) -> Result<()> {
    let mut db = store.cache()?;
    let file = db.file_id();
    let tx = storage_result(db.transaction())?;
    begin_read_snapshot(
        &tx,
        &store.roots.index_db(&store.identity),
        Duration::from_secs(5),
    )?;
    validate(&tx, file)
}

fn commit_on_another_connection(store: &Store, sql: &str) {
    let db = store.cache_write().unwrap();
    assert_eq!(db.execute(sql, []).unwrap(), 1);
}

#[test]
fn an_unchanged_committed_state_is_validated_once_per_scope() {
    let (_state, _root, store) = fresh_store();
    let before = validations(&store);
    let baseline = store.recovery_index_baseline().unwrap();
    assert!(baseline.compatible);
    assert!(store.recorded_index_options().unwrap().is_some());
    let preflight = store.publication_index_baseline().unwrap();
    assert!(preflight.compatible);
    assert_eq!(validations(&store) - before, 2, "Changing and Preflight");
    for _ in 0..3 {
        assert_eq!(store.recovery_index_baseline().unwrap().pin, baseline.pin);
        assert!(store.recorded_index_options().unwrap().is_some());
        assert!(store.publication_index_baseline().unwrap().compatible);
    }
    assert_eq!(
        validations(&store) - before,
        2,
        "no repeat on the same state"
    );
    // A clone shares the memo, like the published-head admission.
    let clone = store.clone();
    assert!(clone.recovery_index_baseline().unwrap().compatible);
    assert_eq!(validations(&store) - before, 2);
}

#[test]
fn a_commit_by_another_connection_validates_again() {
    let (_state, _root, store) = fresh_store();
    let counter = || {
        let mut counter = None;
        read_snapshot(&store, |db, _| {
            counter = Some(published_admission::sqlite_file_change_counter(db)?);
            Ok(())
        })
        .unwrap();
        counter.unwrap()
    };
    assert!(store.recovery_index_baseline().unwrap().compatible);
    let before = validations(&store);
    let first = counter();
    // A COMMIT that writes no page leaves the file, its counter and the memo unchanged.
    commit_on_another_connection(
        &store,
        "UPDATE index_metadata SET indexed_at=indexed_at WHERE singleton=1",
    );
    assert_eq!(counter(), first);
    assert!(store.recovery_index_baseline().unwrap().compatible);
    assert_eq!(validations(&store) - before, 0);
    // A COMMIT that changes the file advances the counter, so the state validates again.
    commit_on_another_connection(
        &store,
        "UPDATE index_metadata SET indexed_at='2026-11-20T00:00:00Z' WHERE singleton=1",
    );
    assert_ne!(counter(), first);
    assert!(store.recovery_index_baseline().unwrap().compatible);
    assert_eq!(validations(&store) - before, 1);
    assert!(store.recovery_index_baseline().unwrap().compatible);
    assert_eq!(validations(&store) - before, 1);
    // A remembered pass never hides a later committed corruption.
    commit_on_another_connection(&store, CORRUPT_INVENTORY);
    assert!(!store.recovery_index_baseline().unwrap().compatible);
    assert_eq!(validations(&store) - before, 2);
    assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
}

#[test]
fn a_reopened_store_or_another_file_validates_again() {
    let (state, root, store) = fresh_store();
    assert!(store.recovery_index_baseline().unwrap().compatible);
    assert!(store.recovery_index_baseline().unwrap().compatible);
    assert_eq!(validations(&store), 1);
    let reopened =
        crate::testing::retry_admission(|| Store::open_for_tests(state.path(), root.path()));
    assert!(reopened.recovery_index_baseline().unwrap().compatible);
    assert_eq!(validations(&reopened), 1, "a reopened Store starts empty");
    assert_eq!(validations(&store), 1);
    // The same counter in another file (device, inode) is another state.
    read_snapshot(&store, |db, (device, inode)| {
        store.validate_bounded_control_once(
            db,
            Some((device, inode.wrapping_add(1))),
            PairedManifestScope::Changing,
        )
    })
    .unwrap();
    assert_eq!(validations(&store), 2);
}

#[test]
fn a_failed_validation_is_never_remembered() {
    let (_state, _root, store) = fresh_store();
    commit_on_another_connection(&store, CORRUPT_INVENTORY);
    let before = validations(&store);
    for attempt in 1..=2 {
        let error = read_snapshot(&store, |db, file| {
            store.validate_bounded_control_once(db, Some(file), PairedManifestScope::Changing)
        })
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("incompatible_index"),
            "{error:#}"
        );
        assert_eq!(validations(&store) - before, attempt);
    }
}

#[test]
fn only_read_only_snapshots_of_a_known_file_use_the_memo() {
    let (_state, _root, store) = fresh_store();
    // Writable and file-less validations neither use nor fill the memo.
    let mut db = store.cache_write().unwrap();
    let file = db.file_id();
    let tx = db.transaction().unwrap();
    store
        .validate_bounded_control_once(&tx, Some(file), PairedManifestScope::Changing)
        .unwrap();
    drop(tx);
    drop(db);
    read_snapshot(&store, |db, _| {
        store.validate_bounded_control_once(db, None, PairedManifestScope::Changing)
    })
    .unwrap();
    assert_eq!(validations(&store), 0);
    assert!(store.recovery_index_baseline().unwrap().compatible);
    assert_eq!(validations(&store), 1);
}
