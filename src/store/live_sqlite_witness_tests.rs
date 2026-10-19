use super::*;
use std::process::Command;

// A separate OS process must never get this writer lock until the first
// process commits. A successful child write is proof that a same-process
// independent check-handle close erased SQLite's POSIX fcntl locks.
#[test]
fn sqlite_lock_contender_child() {
    let Some(path) = std::env::var_os("TRELLIS_LIVE_SQLITE_LOCK_CHILD") else {
        return;
    };
    let db = Connection::open(path).unwrap();
    db.busy_timeout(Duration::ZERO).unwrap();
    let update = match std::env::var("TRELLIS_LIVE_SQLITE_LOCK_TABLE").as_deref() {
        Ok("queue") => "UPDATE queue_identity SET root_key=root_key",
        Ok("index") => "UPDATE index_metadata SET last_opened_at=last_opened_at",
        _ => std::process::exit(19),
    };
    match db.execute_batch(&format!("BEGIN IMMEDIATE; {update}; COMMIT")) {
        Err(rusqlite::Error::SqliteFailure(info, _))
            if matches!(
                info.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) => {}
        Ok(()) => std::process::exit(17),
        Err(_) => std::process::exit(19),
    }
}

fn contend(path: &Path) -> i32 {
    Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("store::live_sqlite_witness_tests::sqlite_lock_contender_child")
        .env("TRELLIS_LIVE_SQLITE_LOCK_CHILD", path)
        .env(
            "TRELLIS_LIVE_SQLITE_LOCK_TABLE",
            if path.file_name().is_some_and(|name| name == "requests.db") {
                "queue"
            } else {
                "index"
            },
        )
        .output()
        .unwrap()
        .status
        .code()
        .unwrap()
}

fn index_fixture() -> (
    tempfile::TempDir,
    tempfile::TempDir,
    Store,
    IndexPin,
    Arc<topology::WriterSession>,
) {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("a.js"),
        "function a() {}
",
    )
    .unwrap();
    let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
    let (pin, owner) = crate::index_coordinator::reconcile_workspace(
        &store,
        &crate::indexer::IndexOptions::new(workspace.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    (state, workspace, store, pin, owner)
}

#[test]
fn request_read_does_not_close_another_live_sqlite_writers_locks() {
    let (_state, workspace, store, _, _owner) = index_fixture();
    let ack = store
        .enqueue_request(
            &crate::indexer::IndexOptions::new(workspace.path().to_owned()),
            None,
        )
        .unwrap();
    let path = store.request_db_path();
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE requests SET submitted_at=submitted_at")
        .unwrap();
    let checked = store.clone();
    let id = ack.id.clone();
    std::thread::spawn(move || checked.request_by_id(&id).unwrap().unwrap())
        .join()
        .unwrap();
    let child = contend(&path);
    let committed = writer.execute_batch("COMMIT");
    assert_eq!(
        child, 0,
        "a second process stole the requests.db writer lock (child exit {child}); first COMMIT: {committed:?}"
    );
    committed.unwrap();
    let persisted = store.request_by_id(&ack.id).unwrap().unwrap();
    assert_eq!(
        (persisted.seq, persisted.state.as_str()),
        (ack.seq, "queued"),
        "previously accepted ACK must survive intact"
    );
}

fn index_check_retains_writer_lock(check: impl FnOnce(&Path) + Send) {
    let (_state, _workspace, store, pin, _owner) = index_fixture();
    let path = store.roots.index_db(&store.identity);
    let writer = Connection::open(&path).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE index_metadata SET last_opened_at=last_opened_at")
        .unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| check(&path)).join().unwrap();
    });
    let child = contend(&path);
    let committed = writer.execute_batch("COMMIT");
    assert_eq!(
        child, 0,
        "a second process stole the index.db writer lock (child exit {child}); first COMMIT: {committed:?}"
    );
    committed.unwrap();
    assert_eq!(
        store.status().unwrap().revision,
        pin,
        "selected retained publication pin must survive index check"
    );
}

#[test]
fn verify_index_file_does_not_close_a_live_sqlite_writers_locks() {
    index_check_retains_writer_lock(|path| verify_index_file(path).unwrap());
}

#[test]
fn index_file_witness_drop_does_not_close_a_live_sqlite_writers_locks() {
    index_check_retains_writer_lock(|path| {
        let witness = IndexFileWitness::open(path).unwrap();
        witness.verify().unwrap();
        drop(witness);
    });
}
