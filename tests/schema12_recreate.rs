//! A physical schema-12 cache is disposable, not migrated in place.
#![cfg(feature = "test-support")]

mod common;

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    index_coordinator::reconcile_workspace,
    indexer::IndexOptions,
    store::{Store, topology::WorkspaceIdentity},
};

#[test]
fn schema12_marker_recreates_exclusively_with_spooled_revision() {
    let (state, roots) = common::fixture();
    let root = state.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("hello.js"), "function hello() { return 1; }\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    // Start with a real durable request queue. Recreate must preserve its
    // accepted rows; a missing queue is not a virgin startup after this point.
    let initial = Store::open_for_tests(state.path(), &root).unwrap();
    let queued = initial
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    drop(initial);
    let path = roots.index_db(&identity);
    fs::remove_file(&path).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(include_str!("fixtures/schema12-new-index.sql"))
        .unwrap();
    db.pragma_update(None, "user_version", 12_i64).unwrap();
    db.execute(
        "INSERT INTO index_metadata VALUES(1,12,'native-v6-delta-v1',?1,?2,?3,?4,0,0,'','{}','[]',NULL)",
        rusqlite::params![
            identity.root.to_str().unwrap(),
            identity.device.to_string(),
            identity.inode.to_string(),
            uuid::Uuid::new_v4().to_string()
        ],
    ).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        12
    );
    assert_eq!(
        db.query_row("SELECT schema_version FROM index_metadata", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        12
    );
    drop(db);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let old_inode = fs::metadata(&path).unwrap().ino();
    let old_bytes = fs::read(&path).unwrap();
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    assert_eq!(
        (report.derived[0].status, report.derived[0].reason),
        ("eligible", "old_schema")
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        old_bytes,
        "a GC report must not mutate an obsolete index"
    );

    let store = Store::open_for_tests(state.path(), &root).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().ino(),
        old_inode,
        "open defers recreation until exclusive publication"
    );
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("recovery_required")
    );
    let (pin, _) = reconcile_workspace(
        &store,
        &IndexOptions::new(root.clone()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(pin.index_revision, 1);
    let preserved = store.request_by_id(&queued.id).unwrap().unwrap();
    assert_eq!(preserved.seq, queued.seq);
    assert_eq!(preserved.state, "queued");
    assert_ne!(fs::metadata(&path).unwrap().ino(), old_inode);
    let rebuilt = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        rebuilt
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        30
    );
    assert_eq!(
        rebuilt
            .query_row("SELECT schema_version FROM index_metadata", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        30
    );
    assert_eq!(
        rebuilt
            .query_row("SELECT index_revision FROM index_metadata", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
