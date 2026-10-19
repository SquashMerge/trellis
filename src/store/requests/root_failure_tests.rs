use super::*;
use std::sync::mpsc;

#[test]
fn root_loss_serializes_prior_accept_and_refuses_late_insert() {
    let state = tempfile::tempdir().unwrap();
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    fs::create_dir(&root).unwrap();
    let store = Store::open_for_tests(state.path(), &root).unwrap();
    let options = IndexOptions::new(root.clone());
    let accepted = store.enqueue_request(&options, None).unwrap();
    let _owner = store.writer_session().unwrap();
    let (locked_tx, locked_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let worker_store = store.clone();
    let worker_options = options.clone();
    let worker = std::thread::spawn(move || {
        worker_store.enqueue_request_with_hook(&worker_options, None, || {
            locked_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
        })
    });
    // The late request already holds SQLite's writer lock. Move the root
    // before allowing its *in-transaction* identity check to run.
    locked_rx.recv().unwrap();
    fs::rename(&root, parent.path().join("old-workspace")).unwrap();
    resume_tx.send(()).unwrap();
    assert!(worker.join().unwrap().is_err());
    assert_eq!(store.fail_changed_root_requests().unwrap(), 1);
    let db = Connection::open(store.request_db_path()).unwrap();
    let rows: i64 = db
        .query_row("SELECT count(*) FROM requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rows, 1, "no late accepted row escaped the transition");
    let code: String = db
        .query_row(
            "SELECT error_code FROM requests WHERE id=?1",
            [&accepted.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(code, "root_changed");
    assert!(store.enqueue_request(&options, None).is_err());
}

#[test]
fn ready_writer_bootstraps_empty_queue_before_first_reconciliation() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert!(!store.request_db_path().exists());
    let owner = store.writer_session().unwrap();
    // EX bootstrap cannot upgrade the daemon's live SH writer guard.
    assert!(store.is_ready_disposition());
    assert!(
        store
            .settle_foreign_root_requests()
            .unwrap_err()
            .is::<super::super::topology::StorageBusy>()
    );
    drop(owner);
    assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    assert!(store.request_db_path().exists());
    assert!(store.current_request().unwrap().is_none());
}

#[test]
fn existing_v0_with_unknown_view_is_incompatible_even_for_writer() {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let path = store.request_db_path();
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .unwrap();
    let original = (
        file.metadata().unwrap().dev(),
        file.metadata().unwrap().ino(),
    );
    drop(file);
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE VIEW untrusted AS SELECT 1")
        .unwrap();
    assert_eq!(
        db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        0
    );
    drop(db);
    let owner = store.writer_session().unwrap();
    let error = store
        .request_connection_for_root_loss(false, true)
        .err()
        .expect("unknown virgin schema must not be initialized");
    assert_eq!(error.to_string(), "incompatible_queue: schema version");
    drop(owner);
    let named = fs::symlink_metadata(&path).unwrap();
    assert_eq!(
        (named.dev(), named.ino()),
        original,
        "refusal must not replace queue inode"
    );
    let read =
        Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let version: i64 = read
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 0, "refusal must not initialize unknown schema");
    let view: String = read
        .query_row(
            "SELECT name FROM sqlite_master WHERE type='view'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(view, "untrusted");
}

#[test]
fn exceptional_dispositions_cannot_treat_missing_queue_as_empty() {
    for disposition in [
        super::super::RecoveryDisposition::Rebuild,
        super::super::RecoveryDisposition::RecreatePending,
    ] {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let _owner = store.writer_session().unwrap();
        store.mark_recovery(disposition);
        assert!(store.settle_foreign_root_requests().is_err());
        assert!(!store.request_db_path().exists());
    }
}
