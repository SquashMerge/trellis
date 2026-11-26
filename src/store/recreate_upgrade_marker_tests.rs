//! #1324: the earlier-format marker that makes a pending recreate report "recreating index
//! after upgrade" is spent once that recreate is resolved. A later corruption on the same
//! Store is real exceptional recovery and keeps `recovery_required`.
use super::*;
use crate::index_coordinator::reconcile_workspace;
use crate::model::IndexOptions;

const UPGRADE: &str = "index_not_ready: recreating index after upgrade";
const CORRUPT: &str = "recovery_required: exceptional index recovery deferred";

fn reconcile(store: &Store, root: &Path) {
    reconcile_workspace(
        store,
        &IndexOptions::new(root.to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(store.disposition(), RecoveryDisposition::Ready);
}

fn set_user_version(store: &Store, version: u32) {
    let db = rusqlite::Connection::open(store.roots.index_db(&store.identity)).unwrap();
    db.pragma_update(None, "user_version", version).unwrap();
}

/// A Store opened on an earlier-schema index, refusing as the upgrade's recreate. When
/// `restored`, the file is current again before the recreate runs, so the exclusive probe
/// finds it opens and rebuilds it in place instead of replacing it.
fn upgrade_resolved(restored: bool) -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("f0.js"), "function f0() {}\n").unwrap();
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(state.path(), root.path()));
    reconcile(&store, root.path());
    set_user_version(&store, 7);
    drop(store);
    let store =
        crate::testing::retry_admission(|| Store::open_for_tests(state.path(), root.path()));
    assert_eq!(store.status().unwrap_err().to_string(), UPGRADE);
    if restored {
        set_user_version(&store, DATABASE_SCHEMA_VERSION);
    }
    reconcile(&store, root.path());
    store.status().unwrap();
    (state, root, store)
}

/// Corruption a read finds on the same Store schedules an exceptional recreate.
fn assert_later_corruption_keeps_recovery_required(store: &Store) {
    let corrupt = anyhow::Error::from(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
        None,
    ));
    assert_eq!(
        store.note_index_failure(&corrupt),
        RecoveryClass::RecreatePending
    );
    assert_eq!(store.disposition(), RecoveryDisposition::RecreatePending);
    for refusal in [
        store.status().unwrap_err(),
        store.index_baseline().unwrap_err(),
    ] {
        assert_eq!(refusal.to_string(), CORRUPT);
    }
}

#[test]
fn corruption_after_the_upgrade_recreate_keeps_recovery_required() {
    let (_state, _root, store) = upgrade_resolved(false);
    assert_later_corruption_keeps_recovery_required(&store);
}

#[test]
fn corruption_after_an_in_place_rebuild_keeps_recovery_required() {
    let (_state, _root, store) = upgrade_resolved(true);
    assert_later_corruption_keeps_recovery_required(&store);
}
