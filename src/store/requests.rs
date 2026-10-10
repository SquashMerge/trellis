//! Durable explicit indexing requests. Each operation has its own short-lived, protected connection.
use super::Store;
use crate::{
    indexer::{IndexOptions, ReconcileOptions},
    model::IndexPin,
    store::topology::{LeaderSession, UseGuard},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::{
    fs,
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

/// A single maintenance unit's existing-only queue admission. Unknown is a
/// priority signal: never begin (or commit) maintenance on uncertain queue data.
pub enum QueueProbeAdmission {
    AbsentVirgin(MaintenanceQueueProbe),
    Ready(MaintenanceQueueProbe),
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MaintenanceQueueState {
    Clear,
    Pending,
    Unknown,
}

/// Keeps the protected SH use lock and one read-only SQLite connection alive
/// only until the current maintenance unit finishes. No SQLite read transaction
/// is held between checks; data_version is meaningful only on this connection.
pub struct MaintenanceQueueProbe {
    identity: Arc<super::topology::WorkspaceIdentity>,
    witness: Arc<Mutex<Option<(u64, u64)>>>,
    path: PathBuf,
    file: Option<Arc<fs::File>>,
    db: Option<super::ProtectedSqliteConnection>,
    _guard: UseGuard,
    inode: Option<(u64, u64)>,
    baseline: i64,
    initial_pending: bool,
}

impl QueueProbeAdmission {
    pub fn check(&self) -> MaintenanceQueueState {
        self.check_with_hook(|| {})
    }

    /// Deterministic contention fixture: runs after the first pathname and
    /// sidecar checks, before reading data_version. Never use a blocking hook
    /// in a live maintenance unit.
    #[doc(hidden)]
    pub fn check_with_hook(&self, after_first_guard: impl FnOnce()) -> MaintenanceQueueState {
        match self {
            Self::Unknown => MaintenanceQueueState::Unknown,
            Self::AbsentVirgin(probe) | Self::Ready(probe) => {
                probe.check_with_barrier(after_first_guard)
            }
        }
    }
}

impl MaintenanceQueueProbe {
    fn pathname_unchanged(&self) -> bool {
        if self.identity.verify_readonly().is_err() || self._guard.verify().is_err() {
            return false;
        }
        let named = match fs::symlink_metadata(&self.path) {
            Ok(named) => Some(named),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return false,
        };
        match (self.inode, named) {
            (None, None) => self.witness.lock().is_ok_and(|w| w.is_none()),
            (Some(inode), Some(named)) => {
                named.is_file()
                    && !named.file_type().is_symlink()
                    && (named.dev(), named.ino()) == inode
                    && self
                        .witness
                        .lock()
                        .is_ok_and(|w| w.is_none_or(|old| old == inode))
            }
            _ => false,
        }
    }

    pub fn check(&self) -> MaintenanceQueueState {
        self.check_with_barrier(|| {})
    }

    fn fence_unchanged(&self, db: Option<&rusqlite::Connection>, version: Option<i64>) -> bool {
        // Sample the second data_version BEFORE the last filesystem check:
        // an uncommitted writer can create a rollback journal while SQLite
        // reads a version that does not change until COMMIT.
        db.is_none_or(|db| {
            db.is_autocommit()
                && version.is_some_and(|version| queue_data_version(db) == Ok(version))
        }) && self
            .file
            .as_ref()
            .is_none_or(|file| queue_delete_header(file))
            && self.pathname_unchanged()
            && !queue_sidecar_exists(&self.path)
    }

    fn check_with_barrier(&self, after_first_guard: impl FnOnce()) -> MaintenanceQueueState {
        if !self.pathname_unchanged() || queue_sidecar_exists(&self.path) {
            return MaintenanceQueueState::Unknown;
        }
        let Some(db) = &self.db else {
            after_first_guard();
            return if self.fence_unchanged(None, None) {
                MaintenanceQueueState::Clear
            } else {
                MaintenanceQueueState::Unknown
            };
        };
        if !self
            .file
            .as_ref()
            .is_some_and(|file| queue_delete_header(file))
            || !db.is_autocommit()
        {
            return MaintenanceQueueState::Unknown;
        }
        after_first_guard();
        let Ok(version) = queue_data_version(db) else {
            return MaintenanceQueueState::Unknown;
        };
        if version == self.baseline {
            // An uncommitted rollback writer does not change data_version.
            // Check journal/identity/header/autocommit AFTER this read too,
            // then sample data_version once more before returning Clear.
            return if self.fence_unchanged(Some(db), Some(version)) {
                if self.initial_pending {
                    MaintenanceQueueState::Pending
                } else {
                    MaintenanceQueueState::Clear
                }
            } else {
                MaintenanceQueueState::Unknown
            };
        }
        let Ok(pending) = unfinished_exists(db) else {
            return MaintenanceQueueState::Unknown;
        };
        if !self.fence_unchanged(Some(db), Some(version)) {
            return MaintenanceQueueState::Unknown;
        }
        if pending {
            MaintenanceQueueState::Pending
        } else {
            MaintenanceQueueState::Clear
        }
    }
}

pub(crate) fn queue_absent_without_sidecars(path: &Path) -> bool {
    matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        && !queue_sidecar_exists(path)
}

fn queue_sidecar_exists(path: &Path) -> bool {
    // A rollback journal may be hot or in use. SQLite can create WAL shared
    // memory files even when the main database is opened read-only; reject
    // any WAL/SHM BEFORE opening SQLite, not only after its first query.
    ["-journal", "-wal", "-shm"].into_iter().any(|suffix| {
        let sidecar = path.with_file_name(format!(
            "{}{}",
            path.file_name().unwrap().to_string_lossy(),
            suffix
        ));
        !matches!(fs::symlink_metadata(sidecar), Err(err) if err.kind() == std::io::ErrorKind::NotFound)
    })
}

fn queue_delete_header(file: &fs::File) -> bool {
    let mut header = [0; 20];
    matches!(file.read_at(&mut header, 0), Ok(20))
        && &header[..16] == b"SQLite format 3\0"
        && header[18] == 1
        && header[19] == 1
}

fn queue_data_version(db: &rusqlite::Connection) -> rusqlite::Result<i64> {
    db.pragma_query_value(None, "data_version", |row| row.get(0))
}

fn unfinished_exists(db: &rusqlite::Connection) -> rusqlite::Result<bool> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM requests INDEXED BY requests_state_seq WHERE state IN ('queued','running') LIMIT 1)",
        [],
        |row| row.get(0),
    )
}

const SCHEMA: &str = "CREATE TABLE requests (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, root_device TEXT NOT NULL, root_inode TEXT NOT NULL, options_json TEXT NOT NULL, expected_generation TEXT, expected_revision INTEGER, state TEXT NOT NULL CHECK(state IN ('queued','running','done','failed')), claim_incarnation TEXT, result_generation TEXT, result_revision INTEGER, error_code TEXT, submitted_at TEXT NOT NULL, started_at TEXT, finished_at TEXT, CHECK ((expected_generation IS NULL) = (expected_revision IS NULL)), CHECK ((result_generation IS NULL) = (result_revision IS NULL)), CHECK ((state='queued' AND claim_incarnation IS NULL AND started_at IS NULL AND finished_at IS NULL AND result_generation IS NULL AND error_code IS NULL) OR (state='running' AND claim_incarnation IS NOT NULL AND started_at IS NOT NULL AND finished_at IS NULL AND result_generation IS NULL AND error_code IS NULL) OR (state='done' AND claim_incarnation IS NOT NULL AND started_at IS NOT NULL AND finished_at IS NOT NULL AND result_generation IS NOT NULL AND error_code IS NULL) OR (state='failed' AND claim_incarnation IS NOT NULL AND started_at IS NOT NULL AND finished_at IS NOT NULL AND result_generation IS NULL AND error_code IS NOT NULL))); CREATE INDEX requests_state_seq ON requests(state,seq); CREATE INDEX requests_root_state_seq ON requests(root_device,root_inode,state,seq); CREATE TABLE queue_identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), root_spelling TEXT NOT NULL, root_key TEXT NOT NULL); PRAGMA user_version=1";

/// GC must not turn the queue's version pragma into permission to unlink an
/// unknown schema. Compare every object, including SQLite-generated indexes.
pub(crate) fn validate_gc_queue(db: &Connection, spelling: &str, key: &str) -> Result<()> {
    type SchemaObject = (String, String, String, Option<String>);
    fn objects(db: &Connection) -> Result<Vec<SchemaObject>> {
        Ok(db
            .prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name")?
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(SCHEMA)?;
    ensure!(
        objects(db)? == objects(&expected)?,
        "incompatible_queue: unknown GC schema"
    );
    let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
    ensure!(version == 1, "incompatible_queue: GC schema version");
    let count: i64 = db.query_row("SELECT count(*) FROM queue_identity", [], |row| row.get(0))?;
    ensure!(count == 1, "incompatible_queue: GC identity cardinality");
    let identity: (String, String) = db.query_row(
        "SELECT root_spelling,root_key FROM queue_identity WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    ensure!(
        identity == (spelling.to_owned(), key.to_owned()),
        "incompatible_queue: GC identity"
    );
    let integrity: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    ensure!(integrity == "ok", "incompatible_queue: GC integrity");
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    #[serde(skip)]
    pub seq: i64,
    pub id: String,
    pub state: String,
    #[serde(skip)]
    pub root_device: String,
    #[serde(skip)]
    pub root_inode: String,
    #[serde(skip)]
    pub options_json: String,
    #[serde(skip)]
    pub expected: Option<IndexPin>,
    #[serde(skip)]
    pub claim_incarnation: Option<String>,
    pub revision: Option<IndexPin>,
    pub error_code: Option<String>,
    pub submitted_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompletionOutcome {
    Done(IndexPin),
    Failed(&'static str),
}
impl CompletionOutcome {
    fn from_result(result: Result<IndexPin>) -> Result<Self> {
        Ok(match result {
            Ok(pin) => Self::Done(pin),
            Err(error) if super::nonterminal_storage_busy(&error) => return Err(error),
            Err(error) if error.to_string().starts_with("revision conflict") => {
                Self::Failed("revision_conflict")
            }
            Err(error) => {
                // A terminal row intentionally retains only a stable error code.
                // Opt-in diagnostics preserve the cause in either process's log
                // without letting a broken stderr change the terminal outcome.
                if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() == Ok("1") {
                    use std::io::Write;
                    let _ = writeln!(std::io::stderr(), "index-phase request_failure={error:#}");
                }
                Self::Failed("index_failed")
            }
        })
    }
    fn matches_terminal(&self, row: &Request) -> bool {
        match self {
            Self::Done(pin) => {
                row.state == "done" && row.revision == Some(*pin) && row.error_code.is_none()
            }
            Self::Failed(code) => {
                row.state == "failed"
                    && row.error_code.as_deref() == Some(*code)
                    && row.revision.is_none()
            }
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct PendingCompletion {
    request: Request,
    outcome: CompletionOutcome,
    incarnation: String,
}

impl Request {
    pub fn options(&self, root: &Path) -> Result<IndexOptions> {
        ensure!(
            self.options_json.len() <= 4096,
            "invalid request options length"
        );
        let value: ReconcileOptions = serde_json::from_str(&self.options_json)?;
        ensure!(
            value.version == 1 && (1..=16_777_216).contains(&value.max_file_bytes),
            "invalid request options"
        );
        value.require_absolute_optional_inputs()?;
        let mut options = IndexOptions::new(root.to_owned());
        options.max_file_bytes = value.max_file_bytes;
        options.scip_path = value.scip_path.map(Into::into);
        options.manifest_path = value.manifest_path.map(Into::into);
        Ok(options)
    }
}
fn now() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}
fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Request> {
    let generation: Option<String> = row.get(5)?;
    let revision: Option<i64> = row.get(6)?;
    let result_generation: Option<String> = row.get(9)?;
    let result_revision: Option<i64> = row.get(10)?;
    let pin = |generation: Option<String>,
               revision: Option<i64>|
     -> rusqlite::Result<Option<IndexPin>> {
        match (generation, revision) {
            (None, None) => Ok(None),
            (Some(g), Some(r)) if r >= 0 => Ok(Some(IndexPin {
                index_generation: Uuid::parse_str(&g).map_err(|_| rusqlite::Error::InvalidQuery)?,
                index_revision: r as u64,
            })),
            _ => Err(rusqlite::Error::InvalidQuery),
        }
    };
    let request = Request {
        seq: row.get(0)?,
        id: row.get(1)?,
        root_device: row.get(2)?,
        root_inode: row.get(3)?,
        options_json: row.get(4)?,
        expected: pin(generation, revision)?,
        state: row.get(7)?,
        claim_incarnation: row.get(8)?,
        revision: pin(result_generation, result_revision)?,
        error_code: row.get(11)?,
        submitted_at: row.get(12)?,
        started_at: row.get(13)?,
        finished_at: row.get(14)?,
    };
    if Uuid::parse_str(&request.id).is_err()
        || !matches!(
            request.state.as_str(),
            "queued" | "running" | "done" | "failed"
        )
        || request.options_json.len() > 4096
        || request.seq < 1
    {
        return Err(rusqlite::Error::InvalidQuery);
    }
    Ok(request)
}
const COLUMNS: &str = "seq,id,root_device,root_inode,options_json,expected_generation,expected_revision,state,claim_incarnation,result_generation,result_revision,error_code,submitted_at,started_at,finished_at";
fn attest_prior_root_rows(db: &Connection, seen: &[Request]) -> Result<bool> {
    for prior in seen {
        let row = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE seq=?1"),
                [prior.seq],
                read,
            )
            .optional()?;
        if !row.is_some_and(|row| {
            row.id == prior.id
                && row.seq == prior.seq
                && row.root_device == prior.root_device
                && row.root_inode == prior.root_inode
                && row.state == "failed"
                && row.error_code.as_deref() == Some("root_changed")
                && row.started_at.is_some()
                && row.finished_at.is_some()
        }) {
            return Ok(false);
        }
    }
    Ok(true)
}

// Verify both the exact rows seen before COMMIT and the absence of a later
// accepted foreign-root row. A terminal row must keep its original ID/seq/root.
fn attest_foreign_root_settlement(
    db: &Connection,
    affected: &[Request],
    incarnation: &str,
    device: &str,
    inode: &str,
) -> Result<bool> {
    for prior in affected {
        let row = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE seq=?1"),
                [prior.seq],
                read,
            )
            .optional()?;
        if !row.is_some_and(|row| {
            row.id == prior.id
                && row.seq == prior.seq
                && row.root_device == prior.root_device
                && row.root_inode == prior.root_inode
                && row.state == "failed"
                && row.error_code.as_deref() == Some("root_changed")
                && row.claim_incarnation.as_deref() == Some(incarnation)
                && row.started_at.is_some()
                && row.finished_at.is_some()
        }) {
            return Ok(false);
        }
    }
    let foreign: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM requests WHERE state IN ('queued','running') AND (root_device<>?1 OR root_inode<>?2))",
        params![device, inode],
        |row| row.get(0),
    )?;
    Ok(!foreign)
}
impl Store {
    #[cfg(test)]
    pub(crate) fn set_queue_select_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.test_queue_select_hook.set(hook);
    }
    #[cfg(test)]
    pub(crate) fn set_exclusive_recovery_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.test_exclusive_recovery_hook.set(hook);
    }
    pub fn request_db_path(&self) -> std::path::PathBuf {
        self.roots.requests_db(&self.identity)
    }
    /// Admit only an existing, known-clean queue for one short maintenance
    /// unit. Never create the queue, recover its journal, wait three seconds,
    /// run quick_check, or acquire a SQLite writer lock.
    pub fn open_maintenance_queue_probe(&self) -> Result<QueueProbeAdmission> {
        Ok(self
            .try_open_maintenance_queue_probe()
            .unwrap_or(QueueProbeAdmission::Unknown))
    }

    fn try_open_maintenance_queue_probe(&self) -> Result<QueueProbeAdmission> {
        self.identity.verify_readonly()?;
        let guard = self.roots.index_use_existing_readonly(&self.identity)?;
        let path = self.request_db_path();
        let probe = |file, db, inode, baseline, initial_pending| MaintenanceQueueProbe {
            identity: self.identity.clone(),
            witness: self.request_file_witness.clone(),
            path: path.clone(),
            file,
            _guard: guard,
            db,
            inode,
            baseline,
            initial_pending,
        };
        // Even an absent main pathname does not make orphan sidecars benign.
        ensure!(!queue_sidecar_exists(&path), "storage_busy: queue sidecar");
        let named = match fs::symlink_metadata(&path) {
            Ok(named) => named,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                ensure!(
                    self.request_file_witness.lock().unwrap().is_none(),
                    "incompatible_queue: previously observed requests.db disappeared"
                );
                let empty = probe(None, None, None, 0, false);
                return Ok(if empty.pathname_unchanged() {
                    QueueProbeAdmission::AbsentVirgin(empty)
                } else {
                    QueueProbeAdmission::Unknown
                });
            }
            Err(err) => return Err(err.into()),
        };
        ensure!(
            named.is_file()
                && !named.file_type().is_symlink()
                && named.uid() == unsafe { libc::geteuid() }
                && named.mode() & 0o777 == 0o600
                && named.nlink() == 1,
            "unsafe requests.db"
        );
        let inode = (named.dev(), named.ino());
        ensure!(
            self.request_file_witness
                .lock()
                .unwrap()
                .is_none_or(|old| old == inode),
            "incompatible_queue: requests.db inode replaced"
        );
        ensure!(!queue_sidecar_exists(&path), "storage_busy: queue sidecar");
        // Register a process-lifetime witness before SQLite opens. Never close
        // another descriptor on an inode while a SQLite connection may hold
        // POSIX record locks on it.
        let file = super::retained_sqlite_file(&path, false, false, true)?;
        let held = file.metadata()?;
        ensure!(
            (held.dev(), held.ino()) == inode,
            "unsafe requests.db changed"
        );
        ensure!(
            queue_delete_header(&file),
            "storage_busy: queue journal format"
        );
        ensure!(!queue_sidecar_exists(&path), "storage_busy: queue sidecar");
        let db = super::protected_sqlite_open(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(std::time::Duration::ZERO)?;
        ensure!(db.is_autocommit(), "storage_busy: queue snapshot active");
        let journal_mode: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        ensure!(
            journal_mode == "delete",
            "storage_busy: queue journal format"
        );
        let version: i64 = db.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(version == 1, "storage_busy: unknown or virgin queue schema");
        let queue_identity: (String, String) = db.query_row(
            "SELECT root_spelling,root_key FROM queue_identity WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            queue_identity == (self.workspace_root.clone(), self.identity.root_key.clone()),
            "root_key_collision: requests.db belongs to different root"
        );
        let baseline = queue_data_version(&db)?;
        let pending = unfinished_exists(&db)?;
        ensure!(
            baseline == queue_data_version(&db)?,
            "storage_busy: queue changed during maintenance admission"
        );
        let opened = probe(Some(file), Some(db), Some(inode), baseline, pending);
        if !opened.pathname_unchanged()
            || queue_sidecar_exists(&path)
            || !opened
                .file
                .as_ref()
                .is_some_and(|file| queue_delete_header(file))
        {
            return Ok(QueueProbeAdmission::Unknown);
        }
        Ok(QueueProbeAdmission::Ready(opened))
    }
    fn request_connection(&self) -> Result<(UseGuard, super::ProtectedSqliteConnection)> {
        // A replacement-root follower may accept before it owns the leader lock,
        // but it must never recreate a missing queue containing old-root ACKs.
        self.request_connection_for_root_loss(false, self.is_root_replaced(), None)
    }
    /// Read an already-admitted queue without creating requests.db or its schema.
    /// A vanished queue we previously observed is not equivalent to a virgin Ready index.
    fn existing_request_connection(
        &self,
    ) -> Result<Option<(UseGuard, super::ProtectedSqliteConnection)>> {
        match self.request_connection_for_root_loss(false, true, None) {
            Ok(pair) => Ok(Some(pair)),
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                ensure!(
                    self.request_file_witness.lock().unwrap().is_none(),
                    "incompatible_queue: previously observed requests.db disappeared"
                );
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    fn request_connection_for_root_loss(
        &self,
        old_root: bool,
        existing_only: bool,
        verified_owner: Option<&LeaderSession>,
    ) -> Result<(UseGuard, super::ProtectedSqliteConnection)> {
        // Root-loss paths keep their existing queue admission barriers.
        #[cfg(test)]
        self.test_queue_before_shared_hook.run();
        if !old_root {
            self.identity.verify()?;
        }
        let guard = if old_root {
            self.roots.index_use_existing_without_root(&self.identity)?
        } else {
            self.roots.index_use(&self.identity)?
        };
        // A waiter may have obtained SH only after the EX replacement
        // published: check the same atomic state while protected, before RW
        // open/create or SQLite journal recovery.
        let path = self.request_db_path();
        // The protected directory and file must remain private, regular and tied to the pathname.
        let file = super::retained_sqlite_file(&path, true, !existing_only, false)?;
        let metadata = file.metadata()?;
        let named = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.is_file()
                && !named.file_type().is_symlink()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && metadata.nlink() == 1
                && (metadata.dev(), metadata.ino()) == (named.dev(), named.ino()),
            "unsafe requests.db"
        );
        // Only the local Arc is released. The process-wide check handle must
        // remain open while any SQLite connection may hold fcntl locks.
        drop(file);
        let mut db = super::protected_sqlite_open(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON")?;
        // A current queue read needs no reserved SQLite writer lock. The first opener
        // alone enters IMMEDIATE, then rechecks after the lock to race safely with
        // another process initializing the same private file. Even read-only callers
        // still open SQLite normally (hot-journal recovery), validate user_version,
        // root singleton and quick_check before interpreting any row.
        let version: i64 =
            super::storage_result(db.pragma_query_value(None, "user_version", |r| r.get(0)))?;
        if version == 0 {
            // Ordinary existing-only readers never initialize a queue: they
            // cannot prove whether a live first writer or a durable ACK owns
            // this inode. Only the current-root Ready owner holding verified EX
            // may recover an existing private virgin file before mandatory H.
            ensure!(
                !existing_only || verified_owner.is_some(),
                "storage_busy: requests.db initialization in progress"
            );
            ensure!(
                self.request_file_witness.lock().unwrap().is_none(),
                "incompatible_queue: previously observed requests.db lost its schema"
            );
            if let Some(owner) = verified_owner {
                ensure!(
                    existing_only && !old_root && self.is_ready_disposition(),
                    "storage_busy: only current Ready owner may recover virgin queue"
                );
                self.verify_leader_session(owner)?;
            }
            // IMMEDIATE waits at most the existing three-second SQLite busy
            // timeout. A concurrent initializer can win; re-read while holding
            // the writer lock instead of assuming an unlocked v0 is orphaned.
            let tx = super::storage_result(
                db.transaction_with_behavior(TransactionBehavior::Immediate),
            )?;
            if let Some(owner) = verified_owner {
                self.verify_leader_session(owner)?;
            }
            let locked_version: i64 =
                super::storage_result(tx.pragma_query_value(None, "user_version", |r| r.get(0)))?;
            if locked_version == 0 {
                let integrity: String =
                    super::storage_result(tx.query_row("PRAGMA quick_check(1)", [], |r| r.get(0)))?;
                ensure!(
                    integrity == "ok",
                    "incompatible_queue: virgin integrity check failed"
                );
                // Count ALL sqlite_master entries, not only user tables: an
                // unexpected view/trigger/index or SQLite-owned object is not
                // an empty queue and must never be overwritten as bootstrap.
                let objects: i64 = super::storage_result(tx.query_row(
                    "SELECT count(*) FROM sqlite_master",
                    [],
                    |r| r.get(0),
                ))?;
                ensure!(objects == 0, "incompatible_queue: unexpected virgin schema");
                if let Some(owner) = verified_owner {
                    self.verify_leader_session(owner)?;
                }
                super::storage_result(tx.execute_batch(SCHEMA))?;
                super::storage_result(tx.execute(
                    "INSERT INTO queue_identity VALUES (1,?1,?2)",
                    params![self.workspace_root, self.identity.root_key],
                ))?;
            } else {
                ensure!(locked_version == 1, "incompatible_queue: schema version");
            }
            if let Some(owner) = verified_owner {
                self.verify_leader_session(owner)?;
            }
            super::storage_result(tx.commit())?;
        } else {
            ensure!(version == 1, "incompatible_queue: schema version");
        }
        let identity: (String, String) = db.query_row(
            "SELECT root_spelling,root_key FROM queue_identity WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            identity == (self.workspace_root.clone(), self.identity.root_key.clone()),
            "root_key_collision: requests.db belongs to different root"
        );
        let check: String = db.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
        ensure!(check == "ok", "incompatible_queue: integrity check failed");
        // Retain an in-process inode witness across all Store clones. A fresh
        // process relies on the durable queue singleton, not a guessed inode.
        let current = fs::symlink_metadata(&path)?;
        ensure!(
            current.is_file()
                && !current.file_type().is_symlink()
                && (current.dev(), current.ino()) == (metadata.dev(), metadata.ino()),
            "unsafe requests.db changed during open"
        );
        let mut witness = self.request_file_witness.lock().unwrap();
        let current_inode = (current.dev(), current.ino());
        ensure!(
            witness.is_none_or(|prior| prior == current_inode),
            "incompatible_queue: requests.db inode replaced"
        );
        *witness = Some(current_inode);
        drop(witness);
        if !old_root {
            self.identity.verify()?;
        }
        guard.verify()?;
        Ok((guard, db))
    }
    pub fn enqueue_request(
        &self,
        options: &IndexOptions,
        expected: Option<IndexPin>,
    ) -> Result<Request> {
        self.enqueue_request_with_hook(options, expected, || {})
    }
    fn enqueue_request_with_hook(
        &self,
        options: &IndexOptions,
        expected: Option<IndexPin>,
        after_write_lock: impl FnOnce(),
    ) -> Result<Request> {
        self.enqueue_request_with_hooks(options, expected, after_write_lock, || {})
    }
    /// Pause a real protected queue write after INSERT but before COMMIT. The
    /// rollback journal is live here; the row is not yet accepted or visible.
    #[doc(hidden)]
    pub fn enqueue_request_after_insert_for_tests(
        &self,
        options: &IndexOptions,
        expected: Option<IndexPin>,
        after_insert: impl FnOnce(),
    ) -> Result<Request> {
        self.enqueue_request_with_hooks(options, expected, || {}, after_insert)
    }
    fn enqueue_request_with_hooks(
        &self,
        options: &IndexOptions,
        expected: Option<IndexPin>,
        after_write_lock: impl FnOnce(),
        after_insert: impl FnOnce(),
    ) -> Result<Request> {
        self.identity.verify()?;
        let selected = std::fs::symlink_metadata(&options.workspace_root)?;
        ensure!(
            selected.is_dir()
                && !selected.file_type().is_symlink()
                && (selected.dev(), selected.ino()) == (self.identity.device, self.identity.inode)
                && std::fs::canonicalize(&options.workspace_root)? == self.identity.root,
            "root_changed: request workspace mismatch"
        );
        let options = ReconcileOptions::from(options);
        ensure!(
            options.version == 1 && (1..=16_777_216).contains(&options.max_file_bytes),
            "invalid request options"
        );
        options.require_absolute_optional_inputs()?;
        let encoded = serde_json::to_string(&options)?;
        ensure!(encoded.len() <= 4096, "invalid request options length");
        let (_guard, mut db) = self.request_connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        after_write_lock();
        self.identity.verify()?;
        let id = Uuid::new_v4().to_string();
        let submitted_at = now();
        let root_device = self.identity.device.to_string();
        let root_inode = self.identity.inode.to_string();
        tx.execute("INSERT INTO requests (id,root_device,root_inode,options_json,expected_generation,expected_revision,state,submitted_at) VALUES (?1,?2,?3,?4,?5,?6,'queued',?7)", params![id,root_device,root_inode,encoded,expected.map(|p|p.index_generation.to_string()),expected.map(|p|p.index_revision as i64),submitted_at])?;
        let seq = tx.last_insert_rowid();
        after_insert();
        tx.commit()?;
        self.identity.verify()?;
        // Return the exact accepted row constructed under the INSERT transaction.
        // A leader may claim immediately after COMMIT; reading it back would race
        // and mislabel the acknowledgement as already running or terminal.
        Ok(Request {
            seq,
            id,
            state: "queued".into(),
            root_device,
            root_inode,
            options_json: encoded,
            expected,
            claim_incarnation: None,
            revision: None,
            error_code: None,
            submitted_at,
            started_at: None,
            finished_at: None,
        })
    }
    pub fn request_by_id(&self, id: &str) -> Result<Option<Request>> {
        if Uuid::parse_str(id).is_err() {
            return Ok(None);
        }
        let Some((_guard, db)) = self.existing_request_connection()? else {
            return Ok(None);
        };
        #[cfg(test)]
        self.test_queue_select_hook.run();
        let row = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [id],
                read,
            )
            .optional()?;
        self.verify_request_root(&row)?;
        Ok(row)
    }
    pub fn earliest_unfinished_request(&self) -> Result<Option<Request>> {
        let Some((_guard, db)) = self.existing_request_connection()? else {
            return Ok(None);
        };
        let row = db.query_row(
            &format!("SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') ORDER BY seq LIMIT 1"),
            [],
            read,
        ).optional()?;
        self.verify_request_root(&row)?;
        Ok(row)
    }
    /// An idle old-root runtime may retire only after its own durable queue is
    /// proven drained. Ordinary request reads require the live root; this one
    /// uses the same protected existing-only queue path as root-loss failure.
    pub fn old_root_unfinished_request(&self) -> Result<Option<Request>> {
        ensure!(
            self.identity.root_path_replaced()?,
            "root_changed: old root required"
        );
        let (_guard, db) = match self.request_connection_for_root_loss(true, true, None) {
            Ok(pair) => pair,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                ensure!(
                    self.request_file_witness.lock().unwrap().is_none(),
                    "incompatible_queue: accepted requests.db disappeared"
                );
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let row = db.query_row(
            &format!("SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') AND root_device=?1 AND root_inode=?2 ORDER BY seq LIMIT 1"),
            params![self.identity.device.to_string(), self.identity.inode.to_string()],
            read,
        ).optional()?;
        ensure!(
            self.identity.root_path_replaced()?,
            "root_changed: old root restored"
        );
        Ok(row)
    }
    pub fn current_request(&self) -> Result<Option<Request>> {
        let Some((_guard, db)) = self.existing_request_connection()? else {
            return Ok(None);
        };
        let row = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE root_device=?1 AND root_inode=?2 ORDER BY seq DESC LIMIT 1"),
                params![self.identity.device.to_string(), self.identity.inode.to_string()],
                read,
            )
            .optional()?;
        self.verify_request_root(&row)?;
        Ok(row)
    }
    fn verify_request_root(&self, row: &Option<Request>) -> Result<()> {
        self.identity.verify()?;
        if let Some(row) = row {
            ensure!(
                row.root_device == self.identity.device.to_string()
                    && row.root_inode == self.identity.inode.to_string(),
                "root_changed: request belongs to another root"
            );
        }
        Ok(())
    }
    /// Mark old-root rows before replacement work, or fail this holder's rows after root loss.
    pub fn fail_changed_root_requests(&self, session: &LeaderSession) -> Result<usize> {
        let old_root = self.identity.root_path_replaced()?;
        if old_root {
            self.verify_old_root_queue_leader(session)?;
        } else {
            self.verify_leader_session(session)?;
            // Before the first durable request, a fresh Ready index has no queue.
            // RootReplaced must never use this exception: its old ACKs may exist.
            if self.is_ready_disposition() {
                match fs::symlink_metadata(self.request_db_path()) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        ensure!(
                            self.request_file_witness.lock().unwrap().is_none(),
                            "incompatible_queue: accepted requests.db disappeared"
                        );
                        return Ok(0);
                    }
                    Err(error) => return Err(error.into()),
                    Ok(_) => {}
                }
            }
        }
        let (_guard, mut db) = self.request_connection_for_root_loss(
            old_root,
            true,
            (!old_root && self.is_ready_disposition()).then_some(session),
        )?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if old_root {
            self.verify_old_root_queue_leader(session)?;
        } else {
            self.verify_leader_session(session)?;
        }
        let predicate = if old_root {
            "root_device=?3 AND root_inode=?4"
        } else {
            "(root_device<>?3 OR root_inode<>?4)"
        };
        let affected: Vec<Request> = {
            let read_predicate = predicate.replace("?3", "?1").replace("?4", "?2");
            let mut statement = tx.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') AND {read_predicate} ORDER BY seq"))?;
            let rows = statement.query_map(
                params![
                    self.identity.device.to_string(),
                    self.identity.inode.to_string()
                ],
                read,
            )?;
            rows.collect::<rusqlite::Result<_>>()?
        };
        let changed = tx.execute(&format!("UPDATE requests SET state='failed',claim_incarnation=?1,started_at=COALESCE(started_at,?2),finished_at=?2,error_code='root_changed' WHERE state IN ('queued','running') AND {predicate}"),
            params![session.incarnation().to_string(),now(),self.identity.device.to_string(),self.identity.inode.to_string()])?;
        ensure!(
            changed == affected.len(),
            "storage_busy: root failure set changed"
        );
        if old_root {
            self.verify_old_root_queue_leader(session)?;
        } else {
            self.verify_leader_session(session)?;
        }
        // A failed COMMIT is ambiguous even if this Connection can reread its
        // writes. Never advance to EX on a result without a successful COMMIT.
        tx.commit()?;
        self.attest_changed_root_rows(&db, &affected, session)?;
        Ok(changed)
    }
    fn attest_changed_root_rows(
        &self,
        db: &Connection,
        affected: &[Request],
        session: &LeaderSession,
    ) -> Result<()> {
        for prior in affected {
            let row = db.query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE seq=?1"),
                [prior.seq],
                read,
            )?;
            ensure!(
                row.id == prior.id
                    && row.root_device == prior.root_device
                    && row.root_inode == prior.root_inode
                    && row.state == "failed"
                    && row.error_code.as_deref() == Some("root_changed")
                    && row.claim_incarnation.as_deref()
                        == Some(session.incarnation().to_string().as_str()),
                "storage_busy: root failure commit not confirmed"
            );
        }
        Ok(())
    }
    fn verify_old_root_queue_leader(&self, session: &LeaderSession) -> Result<()> {
        ensure!(
            self.identity.root_path_replaced()?,
            "root_changed: pathname still names captured root"
        );
        session.verify_after_root_loss(
            &self.identity,
            &self.roots.leader_lock(&self.identity),
            &self.roots.index_use_lock(&self.identity),
        )
    }
    /// Settle every accepted foreign-root row before the new root's H or FIFO claim.
    /// The caller serializes checkout writers. Opening the existing queue first lets
    /// SQLite recover a hot journal before deciding which rows were accepted.
    pub fn settle_foreign_root_requests(&self) -> Result<usize> {
        self.settle_foreign_root_requests_with_hook(|| Ok(()))
    }

    fn settle_foreign_root_requests_with_hook(
        &self,
        after_commit: impl FnOnce() -> Result<()>,
    ) -> Result<usize> {
        let path = self.request_db_path();
        if matches!(fs::symlink_metadata(&path), Err(ref error) if error.kind() == std::io::ErrorKind::NotFound)
        {
            ensure!(
                self.is_ready_disposition() && self.request_file_witness.lock().unwrap().is_none(),
                "incompatible_queue: accepted requests.db disappeared"
            );
            return Ok(0);
        }
        let mut after_commit = Some(after_commit);
        let mut seen = Vec::<Request>::new();
        for _ in 0..2 {
            let (_guard, mut db) = self.request_connection_for_root_loss(false, true, None)?;
            let tx = super::storage_result(
                db.transaction_with_behavior(TransactionBehavior::Immediate),
            )?;
            self.identity.verify()?;
            let affected: Vec<Request> = {
                let mut statement = tx.prepare(&format!("SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') AND (root_device<>?1 OR root_inode<>?2) ORDER BY seq"))?;
                statement
                    .query_map(
                        params![
                            self.identity.device.to_string(),
                            self.identity.inode.to_string()
                        ],
                        read,
                    )?
                    .collect::<rusqlite::Result<_>>()?
            };
            for row in &affected {
                if !seen.iter().any(|prior| prior.seq == row.seq) {
                    seen.push(row.clone());
                }
            }
            let incarnation = Uuid::new_v4().to_string();
            let changed = super::storage_result(tx.execute(
                "UPDATE requests SET state='failed',claim_incarnation=?1,started_at=COALESCE(started_at,?2),finished_at=?2,error_code='root_changed' WHERE state IN ('queued','running') AND (root_device<>?3 OR root_inode<>?4)",
                params![incarnation, now(), self.identity.device.to_string(), self.identity.inode.to_string()],
            ))?;
            ensure!(
                changed == affected.len(),
                "storage_busy: root failure set changed"
            );
            self.identity.verify()?;
            let committed = super::storage_result(tx.commit());
            // A failed COMMIT is ambiguous. Never use the same connection's
            // apparent state as proof: release it and reopen the protected queue.
            drop(db);
            let hook_result = if committed.is_ok() {
                after_commit.take().map_or(Ok(()), |hook| hook())
            } else {
                Ok(())
            };
            let (_guard, reread) = self.request_connection_for_root_loss(false, true, None)?;
            let confirmed = attest_foreign_root_settlement(
                &reread,
                &affected,
                &incarnation,
                &self.identity.device.to_string(),
                &self.identity.inode.to_string(),
            )?;
            if confirmed && attest_prior_root_rows(&reread, &seen)? {
                self.identity.verify()?;
                return Ok(seen.len());
            }
            // A post-COMMIT transport failure is treated like an ambiguous
            // COMMIT. If the reread was incomplete, repeat settlement, not H.
            let _ = (committed, hook_result);
        }
        anyhow::bail!("storage_busy: root settlement not confirmed")
    }

    pub fn claim_request(&self, session: &LeaderSession) -> Result<Option<Request>> {
        // EX ownership alone is not authority to claim. The same incarnation
        // must first commit and attest its selected post-acquisition H.
        self.verify_reconciled_leader_claim(session)?;
        let (_guard, mut db) = self.request_connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.verify_leader_session(session)?;
        let row=tx.query_row(&format!("SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') ORDER BY seq LIMIT 1"), [],read).optional()?;
        let Some(row) = row else { return Ok(None) };
        self.verify_request_root(&Some(row.clone()))?;
        // A running request held by this incarnation belongs to another local driver.
        if row.state == "running"
            && row.claim_incarnation.as_deref() == Some(session.incarnation().to_string().as_str())
        {
            return Ok(None);
        }
        self.verify_reconciled_leader_claim(session)?;
        tx.execute("UPDATE requests SET state='running',claim_incarnation=?1,started_at=?2 WHERE seq=?3 AND state IN ('queued','running')", params![session.incarnation().to_string(),now(),row.seq])?;
        let claimed = tx.query_row(
            &format!("SELECT {COLUMNS} FROM requests WHERE seq=?1"),
            [row.seq],
            read,
        )?;
        tx.commit()?;
        Ok(Some(claimed))
    }
    /// Resolve an operational publish BUSY before retrying an accepted claim.
    /// The observed committed pin must still be the pre-attempt pin. If an
    /// ambiguous COMMIT advanced it, leave the running row for a fresh verified
    /// reconciliation rather than attaching a guessed result or republishing.
    pub(crate) fn requeue_busy_claim(
        &self,
        session: &LeaderSession,
        request: &Request,
        before: Option<IndexPin>,
    ) -> Result<()> {
        self.verify_leader_session(session)?;
        self.fail_changed_root_requests(session)?;
        let current = self.recovery_index_baseline()?.pin();
        let observed = self
            .request_by_id(&request.id)?
            .ok_or_else(|| anyhow::anyhow!("storage_busy: accepted claim disappeared"))?;
        self.verify_request_root(&Some(observed.clone()))?;
        ensure!(
            observed.id == request.id
                && observed.seq == request.seq
                && observed.root_device == request.root_device
                && observed.root_inode == request.root_inode,
            "storage_busy: accepted claim identity changed"
        );
        if observed.finished_at.is_some() {
            if observed.state == "done" {
                let completed = observed
                    .revision
                    .ok_or_else(|| anyhow::anyhow!("storage_busy: terminal pin is missing"))?;
                ensure!(
                    current.is_some_and(|pin| pin.index_generation == completed.index_generation
                        && pin.index_revision >= completed.index_revision),
                    "storage_busy: terminal pin is not retained by current publication"
                );
            }
            // A terminal row is never requeued; caller observes it on reread.
            return Ok(());
        }
        ensure!(
            observed.state == "running"
                && observed.claim_incarnation.as_deref()
                    == Some(session.incarnation().to_string().as_str())
                && request.claim_incarnation == observed.claim_incarnation,
            "storage_busy: request claim changed during busy resolution"
        );
        ensure!(
            current == before,
            "storage_busy: publication changed across failed commit; leave running for verified recovery"
        );
        ensure!(
            !self.has_recorded_completion(session)?,
            "storage_busy: unresolved FIFO completion must not be requeued"
        );
        let (_use_guard, mut db) = self.request_connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.verify_leader_session(session)?;
        self.verify_request_root(&Some(observed))?;
        let changed=tx.execute(
            "UPDATE requests SET state='queued',claim_incarnation=NULL,started_at=NULL WHERE seq=?1 AND id=?2 AND state='running' AND claim_incarnation=?3 AND root_device=?4 AND root_inode=?5",
            params![request.seq,request.id,session.incarnation().to_string(),request.root_device,request.root_inode],
        )?;
        ensure!(
            changed == 1,
            "storage_busy: request claim changed before same-seq requeue"
        );
        self.verify_leader_session(session)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn record_and_finish_request(
        &self,
        session: &LeaderSession,
        request: &Request,
        result: Result<IndexPin>,
    ) -> Result<()> {
        self.verify_leader_session(session)?;
        ensure!(
            request.claim_incarnation.as_deref()
                == Some(session.incarnation().to_string().as_str()),
            "request claim changed"
        );
        {
            let mut slot = self.pending_request_completion.lock().unwrap();
            ensure!(slot.is_none(), "storage_busy: unresolved FIFO completion");
            *slot = Some(PendingCompletion {
                request: request.clone(),
                outcome: CompletionOutcome::from_result(result)?,
                incarnation: session.incarnation().to_string(),
            });
        }
        // Cache the exact result before attempting any SQLite terminal write. A transient
        // failure must not let a subsequent tick re-publish this already executed head.
        self.retry_recorded_completion(session)?;
        Ok(())
    }

    /// A CLI may retry only its own cached completion while holding that same leader.
    /// Do not convert an ordinary claim/read/publish error into an unbounded retry.
    pub(crate) fn has_recorded_completion(&self, session: &LeaderSession) -> Result<bool> {
        Ok(self
            .pending_request_completion
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|pending| pending.incarnation == session.incarnation().to_string()))
    }

    /// Resolve the one in-flight FIFO head before claiming any newer row. A terminal
    /// reread handles an ambiguous SQLite COMMIT; it must match our cached result.
    pub(crate) fn retry_recorded_completion(&self, session: &LeaderSession) -> Result<bool> {
        self.verify_leader_session(session)?;
        let mut slot = self.pending_request_completion.lock().unwrap();
        let Some(pending) = slot.as_ref() else {
            return Ok(false);
        };
        if pending.incarnation != session.incarnation().to_string() {
            // A successor has a new incarnation and must have reconciled before drain.
            // Its ordinary claim path reclaims the old running head under that fence.
            *slot = None;
            return Ok(false);
        }
        let row = self
            .request_by_id(&pending.request.id)?
            .ok_or_else(|| anyhow::anyhow!("storage_busy: cached request disappeared"))?;
        ensure!(
            row.id == pending.request.id
                && row.seq == pending.request.seq
                && row.root_device == pending.request.root_device
                && row.root_inode == pending.request.root_inode
                && row.claim_incarnation.as_deref() == Some(pending.incarnation.as_str()),
            "storage_busy: cached claim changed"
        );
        if row.state == "done" || row.state == "failed" {
            ensure!(
                pending.outcome.matches_terminal(&row),
                "revision conflict: cached completion differs from durable terminal"
            );
            *slot = None;
            return Ok(true);
        }
        ensure!(
            row.state == "running",
            "storage_busy: cached FIFO head is not running"
        );
        if let Err(error) = self.finish_request_outcome(session, &pending.request, &pending.outcome)
        {
            // The write may have committed before returning an error. Confirm the full
            // id/seq/root/incarnation and exact terminal result before treating it as done.
            let reread = self
                .request_by_id(&pending.request.id)?
                .ok_or_else(|| anyhow::anyhow!("storage_busy: cached request disappeared"))?;
            ensure!(
                reread.id == pending.request.id
                    && reread.seq == pending.request.seq
                    && reread.root_device == pending.request.root_device
                    && reread.root_inode == pending.request.root_inode
                    && reread.claim_incarnation.as_deref() == Some(pending.incarnation.as_str()),
                "storage_busy: cached claim changed"
            );
            if reread.state == "done" || reread.state == "failed" {
                ensure!(
                    pending.outcome.matches_terminal(&reread),
                    "revision conflict: cached completion differs from durable terminal"
                );
            } else {
                return Err(error);
            }
        }
        *slot = None;
        Ok(true)
    }

    pub fn finish_request(
        &self,
        session: &LeaderSession,
        request: &Request,
        result: Result<IndexPin>,
    ) -> Result<()> {
        self.finish_request_outcome(session, request, &CompletionOutcome::from_result(result)?)
    }
    fn finish_request_outcome(
        &self,
        session: &LeaderSession,
        request: &Request,
        outcome: &CompletionOutcome,
    ) -> Result<()> {
        self.verify_leader_session(session)?;
        #[cfg(test)]
        if self
            .test_queue_finish_failures
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |count| (count > 0).then(|| count - 1),
            )
            .is_ok()
        {
            anyhow::bail!("storage_busy: injected terminal write failure");
        }
        let (_guard, mut db) = self.request_connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.verify_leader_session(session)?;
        if let CompletionOutcome::Done(pin) = outcome {
            ensure!(
                self.status()?.revision == *pin,
                "revision conflict: request result is not current index"
            );
        }
        let (generation, revision, code) = match outcome {
            CompletionOutcome::Done(pin) => (
                Some(pin.index_generation.to_string()),
                Some(pin.index_revision as i64),
                None,
            ),
            CompletionOutcome::Failed(code) => (None, None, Some(*code)),
        };
        let changed=tx.execute("UPDATE requests SET state=?1,result_generation=?2,result_revision=?3,error_code=?4,finished_at=?5 WHERE seq=?6 AND state='running' AND claim_incarnation=?7 AND root_device=?8 AND root_inode=?9",params![if code.is_some(){"failed"}else{"done"},generation,revision,code,now(),request.seq,session.incarnation().to_string(),self.identity.device.to_string(),self.identity.inode.to_string()])?;
        ensure!(changed == 1, "storage_busy: request claim changed");
        tx.commit()?;
        #[cfg(test)]
        if self
            .test_queue_post_commit_failures
            .try_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |count| (count > 0).then(|| count - 1),
            )
            .is_ok()
        {
            anyhow::bail!("storage_busy: injected ambiguous terminal commit");
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn inject_queue_finish_failure(&self, after_commit: bool) {
        let counter = if after_commit {
            &self.test_queue_post_commit_failures
        } else {
            &self.test_queue_finish_failures
        };
        counter.store(1, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(test)]
mod stale_claim_tests {
    use super::*;
    #[test]
    fn busy_result_can_never_become_terminal_index_failed() {
        for reason in [
            "storage_busy: SQLite lock contention",
            "storage_busy: cached claim changed",
        ] {
            assert!(
                CompletionOutcome::from_result(Err(anyhow::anyhow!(reason))).is_err(),
                "neither transient busy nor invariant busy may mint terminal failure"
            );
        }
    }
    #[test]
    fn stale_claim_cannot_complete_a_row_after_new_leader_reclaims_it() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        fs::write(workspace.path().join("a.js"), "function a() {}\n").unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let (_, old_owner) = crate::index_coordinator::reconcile_workspace(
            &store,
            &options,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let accepted = store.enqueue_request(&options, None).unwrap();
        let stale_claim = store.claim_request(&old_owner).unwrap().unwrap();
        assert_eq!(stale_claim.id, accepted.id);
        drop(old_owner);
        let replacement = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let (new_pin, new_owner) = crate::index_coordinator::reconcile_workspace(
            &replacement,
            &options,
            &std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            |_| {},
        )
        .unwrap();
        let new_claim = replacement.claim_request(&new_owner).unwrap().unwrap();
        assert_ne!(stale_claim.claim_incarnation, new_claim.claim_incarnation);
        let stale_retry = replacement
            .requeue_busy_claim(&new_owner, &stale_claim, Some(new_pin))
            .unwrap_err();
        assert!(
            stale_retry.to_string().contains("claim changed"),
            "{stale_retry:#}"
        );
        let still_new = replacement.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(still_new.state, "running");
        assert_eq!(still_new.claim_incarnation, new_claim.claim_incarnation);
        let error = replacement
            .record_and_finish_request(&new_owner, &stale_claim, Ok(new_pin))
            .unwrap_err();
        assert!(
            error.to_string().contains("request claim changed"),
            "{error:#}"
        );
        let still_running = replacement.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(still_running.state, "running");
        assert_eq!(still_running.claim_incarnation, new_claim.claim_incarnation);
        assert!(still_running.revision.is_none() && still_running.error_code.is_none());
        replacement
            .record_and_finish_request(&new_owner, &new_claim, Ok(new_pin))
            .unwrap();
        assert_eq!(
            replacement
                .request_by_id(&accepted.id)
                .unwrap()
                .unwrap()
                .state,
            "done"
        );
    }
}

#[cfg(test)]
mod root_failure_tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn root_loss_serializes_prior_accept_and_refuses_late_insert_under_writer_lock() {
        let state = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let store = Store::open_for_tests(state.path(), &root).unwrap();
        let options = IndexOptions::new(root.clone());
        let accepted = store.enqueue_request(&options, None).unwrap();
        let owner = store.leader_session().unwrap();
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
        assert_eq!(store.fail_changed_root_requests(&owner).unwrap(), 1);
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
    fn ready_leader_without_accepted_queue_skips_without_creating_db() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert!(!store.request_db_path().exists());
        let owner = store.leader_session().unwrap();
        // Acquiring the leader sets the publication fence; Ready disposition
        // still distinguishes this fresh start from exceptional recovery.
        assert!(store.is_ready_disposition());
        assert_eq!(store.fail_changed_root_requests(&owner).unwrap(), 0);
        assert!(!store.request_db_path().exists());
    }

    #[test]
    fn current_owner_refuses_virgin_version_with_unknown_view_without_replacing_inode() {
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
        let owner = store.leader_session().unwrap();
        let error = store.fail_changed_root_requests(&owner).unwrap_err();
        assert_eq!(
            error.to_string(),
            "incompatible_queue: unexpected virgin schema"
        );
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
            let owner = store.leader_session().unwrap();
            store.mark_recovery(disposition);
            assert!(store.fail_changed_root_requests(&owner).is_err());
            assert!(!store.request_db_path().exists());
        }
    }
}

#[cfg(test)]
mod foreign_root_settlement_tests {
    use super::*;

    #[test]
    fn settles_queued_and_running_foreign_rows_without_touching_current_fifo() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let options = IndexOptions::new(root.path().to_owned());
        let old_queued = store.enqueue_request(&options, None).unwrap();
        let old_running = store.enqueue_request(&options, None).unwrap();
        let current = store.enqueue_request(&options, None).unwrap();
        let db = Connection::open(store.request_db_path()).unwrap();
        db.execute(
            "UPDATE requests SET root_device='old',root_inode='old' WHERE id IN (?1,?2)",
            params![old_queued.id, old_running.id],
        )
        .unwrap();
        db.execute(
            "UPDATE requests SET state='running',claim_incarnation='prior',started_at='1' WHERE id=?1",
            [&old_running.id],
        ).unwrap();
        drop(db);
        assert_eq!(store.settle_foreign_root_requests().unwrap(), 2);
        for accepted in [&old_queued, &old_running] {
            let terminal = store.request_by_id(&accepted.id).unwrap_err();
            assert!(terminal.to_string().contains("root_changed"));
            let db = Connection::open(store.request_db_path()).unwrap();
            let row = db
                .query_row(
                    &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                    [&accepted.id],
                    read,
                )
                .unwrap();
            assert_eq!(row.id, accepted.id);
            assert_eq!(row.seq, accepted.seq);
            assert_eq!(row.state, "failed");
            assert_eq!(row.error_code.as_deref(), Some("root_changed"));
            assert!(row.finished_at.is_some());
        }
        let pending = store.earliest_unfinished_request().unwrap().unwrap();
        assert_eq!((pending.id, pending.seq), (current.id, current.seq));
        assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    }

    #[test]
    fn ambiguous_commit_reopens_and_confirms_terminal_rows() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let accepted = store
            .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
            .unwrap();
        let db = Connection::open(store.request_db_path()).unwrap();
        db.execute(
            "UPDATE requests SET root_inode='old' WHERE id=?1",
            [&accepted.id],
        )
        .unwrap();
        drop(db);
        assert_eq!(
            store
                .settle_foreign_root_requests_with_hook(|| anyhow::bail!(
                    "injected ambiguous COMMIT"
                ))
                .unwrap(),
            1
        );
        let db = Connection::open(store.request_db_path()).unwrap();
        let row = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [&accepted.id],
                read,
            )
            .unwrap();
        assert_eq!(
            (
                row.id,
                row.seq,
                row.state.as_str(),
                row.error_code.as_deref()
            ),
            (accepted.id, accepted.seq, "failed", Some("root_changed"))
        );
    }

    #[test]
    fn missing_virgin_queue_remains_absent() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
        assert!(!store.request_db_path().exists());
    }
}
