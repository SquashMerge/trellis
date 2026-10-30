use super::*;

/// #142: one deadline per read scope. Past it, the busy handler declines
/// at once (no per-statement wait); before it, it waits.
#[test]
fn the_read_scope_busy_handler_follows_the_scope_deadline() {
    let past = Instant::now() - Duration::from_millis(1);
    assert!(!with_read_deadline(past, || read_deadline_busy(0)));
    assert!(!with_read_deadline(past, || read_deadline_busy(7)));
    let future = Instant::now() + Duration::from_secs(60);
    assert!(with_read_deadline(future, || read_deadline_busy(0)));
}

#[test]
fn read_scopes_nest_to_the_earlier_deadline_and_restore() {
    assert_eq!(read_deadline(), None);
    let earlier = Instant::now() + Duration::from_secs(1);
    let later = earlier + Duration::from_secs(1);
    let inner = with_read_deadline(earlier, || with_read_deadline(later, read_deadline));
    assert_eq!(inner, Some(earlier));
    assert_eq!(
        with_read_deadline(later, || with_read_deadline(earlier, read_deadline)),
        Some(earlier)
    );
    assert_eq!(
        read_deadline(),
        None,
        "the scope must restore the previous deadline"
    );
}

/// Holds `BEGIN EXCLUSIVE` on `path` from another connection (no write,
/// so no journal sidecar) until `release` is sent.
fn hold_exclusive(path: &Path) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let path = path.to_path_buf();
    let holder = std::thread::spawn(move || {
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        locked_tx.send(()).unwrap();
        let _ = release_rx.recv();
        writer.execute_batch("ROLLBACK").unwrap();
    });
    locked_rx.recv().unwrap();
    (release_tx, holder)
}

/// Releases the writer's lock shortly after a probe starts: a probe that
/// still waits per statement then succeeds, one bound by an expired read
/// scope has already failed.
fn probe_against_releasing_writer(
    path: &Path,
    writable: bool,
    probe: impl FnOnce(&Path, bool) -> Result<ProtectedSqliteConnection>,
) -> Result<ProtectedSqliteConnection> {
    let (release, holder) = hold_exclusive(path);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let _ = release.send(());
    });
    let opened = probe(path, writable);
    releaser.join().unwrap();
    holder.join().unwrap();
    opened
}

/// #142: the read deadline reaches real index connections. Inside an
/// expired read scope, a read-only `open_index_marker_probe` fails typed
/// instead of waiting out its per-call busy timeout; outside a scope, and
/// for a writable open, it waits as before.
#[test]
fn open_index_marker_probe_applies_the_read_scope_only_to_read_only_opens() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().canonicalize().unwrap().join("index.db");
    {
        let setup = rusqlite::Connection::open(&path).unwrap();
        setup
            .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE witness(id INTEGER)")
            .unwrap();
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let expired = || Instant::now() - Duration::from_millis(1);

    let in_scope = probe_against_releasing_writer(&path, false, |path, writable| {
        with_read_deadline(expired(), || {
            open_index_marker_probe(path, writable, Duration::from_secs(5))
        })
    });
    let busy = in_scope
        .err()
        .expect("a read-only probe in an expired scope must not wait");
    assert!(
        busy.downcast_ref::<SqliteContention>().is_some(),
        "{busy:#}"
    );

    let outside = probe_against_releasing_writer(&path, false, |path, writable| {
        open_index_marker_probe(path, writable, Duration::from_secs(5))
    });
    assert!(
        outside.is_ok(),
        "outside a read scope the probe waits: {:#}",
        outside.err().unwrap()
    );

    let writable = probe_against_releasing_writer(&path, true, |path, writable| {
        with_read_deadline(expired(), || {
            open_index_marker_probe(path, writable, Duration::from_secs(5))
        })
    });
    assert!(
        writable.is_ok(),
        "a writable probe ignores the read scope: {:#}",
        writable.err().unwrap()
    );
}

/// A reader blocked by a writer, in a scope whose deadline has passed,
/// gets typed storage_busy on its first statement instead of waiting.
#[test]
fn an_expired_read_scope_fails_typed_without_waiting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE witness(id INTEGER)")
        .unwrap();
    let mut reader = rusqlite::Connection::open(&path).unwrap();
    reader.busy_handler(Some(read_deadline_busy)).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let busy = with_read_deadline(Instant::now() - Duration::from_millis(1), || {
        storage_result(reader.query_row("SELECT count(*) FROM witness", [], |r| r.get::<_, i64>(0)))
            .unwrap_err()
    });
    assert_eq!(busy.to_string(), "storage_busy: SQLite lock contention");
    assert!(busy.downcast_ref::<SqliteContention>().is_some());
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn rollback_writer_busy_is_typed_without_raw_sqlite_cause_or_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("busy.db");
    let first = rusqlite::Connection::open(&path).unwrap();
    first
        .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE witness(id INTEGER)")
        .unwrap();
    let second = rusqlite::Connection::open(&path).unwrap();
    second.busy_timeout(std::time::Duration::ZERO).unwrap();
    first.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let busy = storage_result(second.execute_batch("BEGIN IMMEDIATE")).unwrap_err();
    assert_eq!(busy.to_string(), "storage_busy: SQLite lock contention");
    assert!(busy.downcast_ref::<SqliteContention>().is_some());
    assert!(
        !busy
            .chain()
            .any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some())
    );
    assert!(transient_storage_contention(&busy));
    assert_eq!(recovery_class(&busy), RecoveryClass::Hard);
    let contextual = busy.context("outer publication context");
    assert!(transient_storage_contention(&contextual));
    assert!(!transient_storage_contention(&anyhow::anyhow!(
        "storage_busy: cached claim changed"
    )));
    first.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn schema_admission_busy_is_typed_and_cannot_mark_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE witness(id INTEGER)")
        .unwrap();
    let reader = rusqlite::Connection::open(&path).unwrap();
    reader.busy_timeout(Duration::ZERO).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let busy = validate_cache_shape(&reader).unwrap_err();
    assert_eq!(busy.to_string(), "storage_busy: SQLite lock contention");
    assert!(busy.downcast_ref::<SqliteContention>().is_some());
    assert_eq!(recovery_class(&busy), RecoveryClass::Hard);
    assert!(
        !busy
            .chain()
            .any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some())
    );
    writer.execute_batch("ROLLBACK").unwrap();
}

/// Pauses the next read-only connection right after SQLite's hot-journal
/// check saw `-journal` exist, before it checks for a RESERVED holder.
static JOURNAL_SEEN_GATE: std::sync::Mutex<
    Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
> = std::sync::Mutex::new(None);
static GATED_BASE_VFS: std::sync::OnceLock<usize> = std::sync::OnceLock::new();

unsafe extern "C" fn gated_access(
    _vfs: *mut rusqlite::ffi::sqlite3_vfs,
    name: *const std::ffi::c_char,
    flags: std::ffi::c_int,
    out: *mut std::ffi::c_int,
) -> std::ffi::c_int {
    let base = *GATED_BASE_VFS.get().unwrap() as *mut rusqlite::ffi::sqlite3_vfs;
    let rc = unsafe { ((*base).xAccess.unwrap())(base, name, flags, out) };
    let journal = unsafe { std::ffi::CStr::from_ptr(name) }
        .to_bytes()
        .ends_with(b"-journal");
    if journal
        && unsafe { *out } != 0
        && let Some((seen, resume)) = JOURNAL_SEEN_GATE.lock().unwrap().take()
    {
        seen.send(()).unwrap();
        resume.recv().unwrap();
    }
    rc
}

/// The default VFS with only `xAccess` wrapped, registered once.
fn gated_vfs() -> &'static str {
    const NAME: &std::ffi::CStr = c"trellis-gated-journal-access";
    GATED_BASE_VFS.get_or_init(|| unsafe {
        let base = rusqlite::ffi::sqlite3_vfs_find(std::ptr::null());
        let mut gated = *base;
        gated.zName = NAME.as_ptr();
        gated.xAccess = Some(gated_access);
        let gated = Box::leak(Box::new(gated));
        assert_eq!(
            rusqlite::ffi::sqlite3_vfs_register(gated, 0),
            rusqlite::ffi::SQLITE_OK
        );
        base as usize
    });
    NAME.to_str().unwrap()
}

/// Runs `read` on a read-only gated connection while `writer` holds a live
/// journal; once SQLite's hot-journal check has seen it, `writer` rolls
/// back, deleting the journal and dropping RESERVED without ever needing
/// EXCLUSIVE.
fn read_racing_writer_rollback<T: Send + 'static>(
    read: impl FnOnce(&Connection, &Path) -> T + Send + 'static,
) -> T {
    // One gate serves every test in this binary: race one reader at a time.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let journal = dir.path().join("index.db-journal");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer
        .execute_batch(
            "PRAGMA journal_mode=DELETE; CREATE TABLE witness(id INTEGER);
                 INSERT INTO witness VALUES (1);",
        )
        .unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE witness SET id=2")
        .unwrap();
    assert!(journal.exists(), "the writer's live journal");
    let vfs = gated_vfs();
    let (seen_tx, seen_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    *JOURNAL_SEEN_GATE.lock().unwrap() = Some((seen_tx, resume_rx));
    let reader_path = path.clone();
    let reader = std::thread::spawn(move || {
        let reader = rusqlite::Connection::open_with_flags_and_vfs(
            &reader_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            vfs,
        )
        .unwrap();
        reader.busy_timeout(Duration::from_secs(5)).unwrap();
        read(&reader, &reader_path)
    });
    seen_rx
        .recv_timeout(crate::testing::HANG_BACKSTOP)
        .expect("reader reached the hot-journal check");
    writer.execute_batch("ROLLBACK").unwrap();
    assert!(!journal.exists(), "rollback deleted the journal");
    resume_tx.send(()).unwrap();
    reader.join().unwrap()
}

type Attempts = Vec<std::result::Result<i64, String>>;

/// One retried read of the witness row, recording every attempt.
fn retried_read(
    budget: Duration,
) -> impl FnOnce(&Connection, &Path) -> (Result<i64>, Attempts) + Send + 'static {
    move |reader, path| {
        let mut attempts = vec![];
        let result = retry_stale_hot_journal(path, budget, || {
            let read = storage_result(
                reader.query_row("SELECT id FROM witness", [], |r| r.get::<_, i64>(0)),
            );
            attempts.push(read.as_ref().map(|id| *id).map_err(|e| e.to_string()));
            read
        });
        (result, attempts)
    }
}

/// #183: a reader racing a writer's rollback gets SQLite's false-positive
/// hot-journal verdict (SQLite ticket #3883). With the journal gone it is
/// contention: the read retries and sees the rolled-back data.
#[test]
fn stale_hot_journal_verdict_racing_a_writer_rollback_is_retried() {
    let (result, attempts) = read_racing_writer_rollback(retried_read(Duration::from_secs(5)));
    assert_eq!(
        attempts,
        vec![Err(HOT_INDEX_JOURNAL.to_string()), Ok(1)],
        "the first read saw the false positive, the retry read"
    );
    assert_eq!(result.unwrap(), 1);
}

/// A read transaction's snapshot is where its hot-journal check runs: it
/// retries the stale verdict and later reads in it need no retry.
#[test]
fn read_snapshot_retries_a_stale_hot_journal_verdict() {
    let id = read_racing_writer_rollback(|reader, path| {
        reader.execute_batch("BEGIN DEFERRED").unwrap();
        begin_read_snapshot(reader, path, Duration::from_secs(5)).unwrap();
        let id =
            storage_result(reader.query_row("SELECT id FROM witness", [], |r| r.get::<_, i64>(0)));
        reader.execute_batch("COMMIT").unwrap();
        id
    });
    assert_eq!(id.unwrap(), 1);
}

/// Maintenance's zero-wait probes take their snapshot with no budget: the
/// stale verdict becomes contention, which defers the maintenance unit.
#[test]
fn zero_wait_read_snapshot_defers_a_stale_hot_journal_verdict() {
    let error = read_racing_writer_rollback(|reader, path| {
        reader.execute_batch("BEGIN DEFERRED").unwrap();
        begin_read_snapshot(reader, path, Duration::ZERO)
    })
    .unwrap_err();
    assert!(
        error.downcast_ref::<SqliteContention>().is_some(),
        "{error:#}"
    );
    assert!(maintenance_contention(&error));
}

/// With no wait budget left, the stale verdict is typed contention, never
/// a recovery requirement.
#[test]
fn stale_hot_journal_verdict_without_budget_is_typed_contention() {
    let (result, attempts) = read_racing_writer_rollback(retried_read(Duration::ZERO));
    assert_eq!(attempts, vec![Err(HOT_INDEX_JOURNAL.to_string())]);
    let error = result.unwrap_err();
    assert!(
        error.downcast_ref::<SqliteContention>().is_some(),
        "{error:#}"
    );
    assert!(transient_storage_contention(&error));
}

/// A journal SQLite still finds hot, with no writer, keeps the typed
/// recovery requirement at once.
#[test]
fn persistent_hot_journal_keeps_its_recovery_verdict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    let writer = rusqlite::Connection::open(&path).unwrap();
    writer
        .execute_batch("PRAGMA journal_mode=DELETE; CREATE TABLE witness(id INTEGER)")
        .unwrap();
    drop(writer);
    std::fs::write(dir.path().join("index.db-journal"), [0xd9u8; 512]).unwrap();
    let reader =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut attempts = 0;
    let error = retry_stale_hot_journal(&path, Duration::from_secs(5), || {
        attempts += 1;
        storage_result(reader.query_row("SELECT count(*) FROM witness", [], |r| r.get::<_, i64>(0)))
    })
    .unwrap_err();
    assert_eq!(attempts, 1);
    assert!(error.downcast_ref::<HotIndexJournal>().is_some());
    assert_eq!(error.to_string(), HOT_INDEX_JOURNAL);
}

#[test]
fn ioerr_rdlock_does_not_convert_to_contention() {
    let error = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_RDLOCK),
        None,
    );
    let reported = storage_result::<()>(Err(error)).unwrap_err();
    assert!(reported.downcast_ref::<rusqlite::Error>().is_some());
    assert!(reported.downcast_ref::<SqliteContention>().is_none());
    assert!(!transient_storage_contention(&reported));
}

/// #220: a writable open admits a cold journal (zeroed header, as a writer
/// killed before its commit phase leaves it) and refuses one SQLite may treat
/// as hot, naming the cause. WAL and shared-memory sidecars stay refused, and
/// a read-only open never checks the journal.
#[test]
fn writable_open_admits_only_a_cold_journal_and_names_each_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    std::fs::write(&path, b"").unwrap();
    let journal = dir.path().join("index.db-journal");
    for cold in [&[][..], &[0u8; 512][..]] {
        std::fs::write(&journal, cold).unwrap();
        reject_sidecars(&path, true).unwrap();
    }
    for hot in [&[0xd9u8; 512][..], &[0x01][..]] {
        std::fs::write(&journal, hot).unwrap();
        let error = reject_sidecars(&path, true).unwrap_err().to_string();
        assert_eq!(
            error,
            format!("{HOT_INDEX_JOURNAL}: {}", journal.display()),
            "{hot:?}"
        );
        reject_sidecars(&path, false).unwrap();
    }
    std::fs::remove_file(&journal).unwrap();
    for suffix in ["-wal", "-shm"] {
        let sidecar = dir.path().join(format!("index.db{suffix}"));
        std::fs::write(&sidecar, [0u8; 32]).unwrap();
        for writable in [true, false] {
            assert_eq!(
                reject_sidecars(&path, writable).unwrap_err().to_string(),
                format!(
                    "recovery_required: unsupported index sidecar: {}",
                    sidecar.display()
                )
            );
        }
        std::fs::remove_file(&sidecar).unwrap();
    }
    reject_sidecars(&path, true).unwrap();
}

/// #221 review: a non-regular journal is refused by type before any byte is
/// read. A FIFO would block a read open indefinitely; a symlink to a cold file
/// must not take the cold allowance. Bounded so a regression fails, not hangs.
#[test]
fn writable_open_refuses_a_non_regular_journal_without_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    std::fs::write(&path, b"").unwrap();
    let journal = dir.path().join("index.db-journal");
    let cold = dir.path().join("cold");
    std::fs::write(&cold, [0u8; 512]).unwrap();
    let fifo = std::ffi::CString::new(journal.as_os_str().as_encoded_bytes()).unwrap();
    let make_fifo: Box<dyn Fn()> = Box::new(move || {
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    });
    let link = journal.clone();
    let make_symlink: Box<dyn Fn()> = Box::new(move || {
        std::os::unix::fs::symlink(&cold, &link).unwrap();
    });
    for (kind, create) in [("fifo", make_fifo), ("symlink", make_symlink)] {
        create();
        let (sent, received) = std::sync::mpsc::channel();
        let checked = path.clone();
        std::thread::spawn(move || {
            let _ = sent.send(reject_sidecars(&checked, true).map_err(|e| e.to_string()));
        });
        let refused = received
            .recv_timeout(Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{kind} journal blocked the writable preflight"));
        assert_eq!(
            refused.unwrap_err(),
            format!(
                "recovery_required: unsupported index sidecar: {}",
                journal.display()
            ),
            "{kind}"
        );
        std::fs::remove_file(&journal).unwrap();
    }
}
