use super::*;
use crate::index_coordinator::{IndexJobCoordinator, reconcile_workspace};
use crate::indexer::IndexOptions;
use std::{
    fs::OpenOptions,
    io::{Seek, SeekFrom, Write},
};

fn published_store() -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("f0.js"), "function f0() {}\n").unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    reconcile_workspace(
        &store,
        &IndexOptions::new(root.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    (state, root, store)
}

fn assert_file_corruption(error: &anyhow::Error, store: &Store) {
    assert!(
        matches!(
            error.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(info, _))
                if info.code == rusqlite::ErrorCode::DatabaseCorrupt
        ),
        "{error:#}"
    );
    assert_eq!(store.disposition(), RecoveryDisposition::RecreatePending);
    assert!(store.recovery_required.load(Ordering::Acquire));
}

fn reconcile_and_read(store: &Store, root: &Path) {
    let (pin, session) = reconcile_workspace(
        store,
        &IndexOptions::new(root.to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(store.disposition(), RecoveryDisposition::Ready);
    assert!(!store.recovery_required.load(Ordering::Acquire));
    let (actual, symbols) = store.symbols_at("f0", 10).unwrap();
    assert_eq!(actual, pin);
    assert!(symbols.iter().any(|symbol| symbol.name == "f0"));
    assert!(
        store
            .source("f0.js")
            .unwrap()
            .unwrap()
            .text
            .contains("function f0")
    );
    drop(session);
}

#[test]
fn corrupt_ordinary_table_root_page_is_recreated_after_a_selected_read() {
    let (_state, root, store) = published_store();
    assert!(!store.symbols("f0", 10).unwrap().is_empty());
    let index = store.roots.index_db(&store.identity);
    let (page_size, root_page): (u32, u32) = {
        let db = store.cache().unwrap();
        let page_size: u32 = db
            .pragma_query_value(None, "page_size", |row| row.get(0))
            .unwrap();
        let root_page: u32 = db
            .query_row(
                "SELECT rootpage FROM sqlite_schema WHERE name='graph_nodes' AND type='table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (page_size, root_page)
    };
    assert!(root_page > 1);
    // No SQLite connection or writer session survives this mutation. The
    // database header stays valid; only this ordinary table's b-tree is broken.
    {
        let mut file = OpenOptions::new().write(true).open(&index).unwrap();
        file.seek(SeekFrom::Start(
            u64::from(root_page - 1) * u64::from(page_size),
        ))
        .unwrap();
        file.write_all(&[0]).unwrap();
        file.sync_all().unwrap();
    }
    let error = store.symbols("f0", 10).unwrap_err();
    assert_file_corruption(&error, &store);
    reconcile_and_read(&store, root.path());
}

#[test]
fn corrupt_fts_delete_during_row_rebuild_escalates_to_recreation() {
    let (_state, root, store) = published_store();
    {
        let db = store.cache_write().unwrap();
        db.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        assert_eq!(
            db.execute("UPDATE document_versions SET byte_length='invalid'", [])
                .unwrap(),
            1
        );
    }
    let row_error = store.source("f0.js").unwrap_err();
    assert_eq!(recovery_class(&row_error), RecoveryClass::Rebuild);
    assert_eq!(store.disposition(), RecoveryDisposition::Rebuild);
    {
        let db = store.cache_write().unwrap();
        assert_eq!(
            db.execute(
                "UPDATE graph_node_trigrams_data SET block=x'ffffffffffffffff' WHERE id=10",
                [],
            )
            .unwrap(),
            1
        );
    }
    // Preparation only reads control state. The full rebuild's DELETE invokes
    // graph_nodes' FTS trigger against the corrupt structure record.
    let job = IndexJobCoordinator::prepare(&store, None).unwrap();
    let error = job
        .run(
            &IndexOptions::new(root.path().to_owned()),
            &Arc::new(AtomicBool::new(false)),
            |_| {},
        )
        .unwrap_err();
    assert_file_corruption(&error, &store);
    reconcile_and_read(&store, root.path());
}

#[test]
fn sqlite_witness_resource_limit_does_not_schedule_recovery() {
    let (_state, _root, store) = published_store();
    let pin = store.status().unwrap().revision;
    let error = store.report_index_failure(anyhow::anyhow!(
        "incompatible_index: verified SQLite file witness capacity reached"
    ));
    assert_eq!(recovery_class(&error), RecoveryClass::Hard);
    assert_eq!(store.disposition(), RecoveryDisposition::Ready);
    assert!(!store.recovery_required.load(Ordering::Acquire));
    assert_eq!(store.status().unwrap().revision, pin);
    assert!(store.source("f0.js").unwrap().is_some());
}

#[test]
fn exceptional_recheck_without_file_damage_rebuilds_in_place() {
    use std::os::unix::fs::MetadataExt;

    let (_state, root, store) = published_store();
    let before = store.status().unwrap().revision;
    let index = store.roots.index_db(&store.identity);
    let inode = std::fs::metadata(&index).unwrap().ino();
    let (page_size, root_page): (u32, u32) = {
        let db = store.cache().unwrap();
        (
            db.pragma_query_value(None, "page_size", |row| row.get(0))
                .unwrap(),
            db.query_row(
                "SELECT rootpage FROM sqlite_schema WHERE name='graph_nodes' AND type='table'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        )
    };
    let offset = u64::from(root_page - 1) * u64::from(page_size);
    let original = std::fs::read(&index).unwrap()[offset as usize];
    {
        let mut file = OpenOptions::new().write(true).open(&index).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&[0]).unwrap();
        file.sync_all().unwrap();
    }
    assert_file_corruption(&store.symbols("f0", 10).unwrap_err(), &store);
    // Repair the triggering page only after EX admission, so the recheck passes
    // without erasing the earlier corruption observation or its recovery fence.
    let restored = index.clone();
    store.test_exclusive_recovery_hook.set(move || {
        let mut file = OpenOptions::new().write(true).open(restored).unwrap();
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(&[original]).unwrap();
        file.sync_all().unwrap();
    });
    reconcile_and_read(&store, root.path());
    let after = store.status().unwrap().revision;
    assert_ne!(before.index_generation, after.index_generation);
    assert_eq!(after.index_revision, 1);
    assert_eq!(std::fs::metadata(index).unwrap().ino(), inode);
}
