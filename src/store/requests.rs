//! Durable explicit indexing requests. Each operation has its own short-lived, protected connection.
use super::Store;
#[cfg(any(test, feature = "test-support"))]
use super::TestTerminalNotify;
use crate::{
    indexer::{IndexOptions, ReconcileOptions},
    model::IndexPin,
    store::topology::{UseGuard, WriterSession},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use std::{
    fs,
    os::unix::fs::{FileExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
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
// The protected opener has legacy raw SQLite calls. Convert only an exact
// SQLite lock conflict when this settlement path invokes it.
fn settlement_open_error(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| matches!(cause.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(info, _))
            if matches!(info.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))) {
        super::SqliteContention(()).into()
    } else {
        error
    }
}

fn attest_prior_root_rows(db: &Connection, seen: &[Request]) -> Result<bool> {
    for prior in seen {
        let row = super::storage_result(
            db.query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE seq=?1"),
                [prior.seq],
                read,
            )
            .optional(),
        )?;
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
        let row = super::storage_result(
            db.query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE seq=?1"),
                [prior.seq],
                read,
            )
            .optional(),
        )?;
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
    let foreign: bool = super::storage_result(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM requests WHERE state IN ('queued','running') AND (root_device<>?1 OR root_inode<>?2))",
        params![device, inode],
        |row| row.get(0),
    ))?;
    Ok(!foreign)
}
/// A normal error or unwind discards the private stage after its SQLite and
/// raw file handles have closed. Process death may leave a harmless sibling.
struct QueueBootstrapStage {
    path: PathBuf,
    published: bool,
}
impl Drop for QueueBootstrapStage {
    fn drop(&mut self) {
        if !self.published {
            for path in [
                self.path.with_file_name(format!(
                    "{}-journal",
                    self.path.file_name().unwrap().to_string_lossy()
                )),
                self.path.with_file_name(format!(
                    "{}-wal",
                    self.path.file_name().unwrap().to_string_lossy()
                )),
                self.path.with_file_name(format!(
                    "{}-shm",
                    self.path.file_name().unwrap().to_string_lossy()
                )),
                self.path.clone(),
            ] {
                let _ = fs::remove_file(path);
            }
        }
    }
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
    /// Prepare a virgin queue before exposing a new checkout runtime to
    /// selected reads. Otherwise repeated cold status reads can hold SH while
    /// mandatory H tries to publish the queue under EX.
    pub(crate) fn prepare_virgin_queue_before_activation(&self) -> Result<()> {
        self.identity.verify()?;
        let path = self.request_db_path();
        match fs::symlink_metadata(&path) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Existing queues and prior heads retain the ordinary H settlement
        // path, so validated prior-head reads remain available during H.
        if !self.is_ready_disposition() || self.index_baseline()?.index_revision != 0 {
            return Ok(());
        }
        ensure!(
            self.request_file_witness.lock().unwrap().is_none(),
            "incompatible_queue: previously observed requests.db disappeared"
        );
        // A sidecar without its main queue is not a virgin queue.
        ensure!(
            !queue_sidecar_exists(&path),
            "incompatible_queue: missing requests.db has sidecars"
        );
        // request_connection itself drops SH before the nonblocking EX stage,
        // then validates v1 and syncs both directories on the reopened inode.
        let (guard, db) = self.request_connection()?;
        drop(db);
        drop(guard);
        Ok(())
    }

    /// Publish only a complete queue. The index-use EX guard excludes GC and
    /// every normal queue opener until the directory entry is durable.
    fn bootstrap_missing_queue(
        &self,
        #[cfg(test)] before_publish: impl FnOnce(&Path) -> Result<()>,
        #[cfg(test)] after_publish: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        use std::os::unix::fs::OpenOptionsExt;

        self.identity.verify()?;
        ensure!(
            self.is_ready_disposition(),
            "incompatible_queue: missing queue outside Ready startup"
        );
        let exclusive = self.roots.index_use_exclusive_existing(&self.identity)?;
        exclusive.verify()?;
        self.identity.verify()?;
        let path = self.request_db_path();
        ensure!(
            self.request_file_witness.lock().unwrap().is_none(),
            "incompatible_queue: previously observed requests.db disappeared"
        );
        // A concurrent initializer may have published while we waited for EX.
        match fs::symlink_metadata(&path) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let parent = path.parent().expect("queue has a parent directory");
        // A crash orphan must not become an unknown entry in the candidate
        // index directory: GC admits only index.db and requests.db there.
        let stage = parent
            .parent()
            .expect("index has a parent directory")
            .join(format!(
                ".requests-{}-{}.stage",
                self.identity.root_key,
                Uuid::new_v4()
            ));
        let mut cleanup = QueueBootstrapStage {
            path: stage.clone(),
            published: false,
        };
        let staged_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&stage)?;
        // This private staging path is intentionally NOT registered in the
        // process-wide SQLite witness table. Close SQLite before syncing, then
        // close this separate raw FD before the inode becomes requests.db.
        {
            let mut db = Connection::open_with_flags(
                &stage,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            db.busy_timeout(std::time::Duration::from_secs(3))?;
            db.execute_batch(
                "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON",
            )?;
            let tx = super::storage_result(
                db.transaction_with_behavior(TransactionBehavior::Immediate),
            )?;
            super::storage_result(tx.execute_batch(SCHEMA))?;
            super::storage_result(tx.execute(
                "INSERT INTO queue_identity VALUES (1,?1,?2)",
                params![self.workspace_root, self.identity.root_key],
            ))?;
            super::storage_result(tx.commit())?;
        }
        staged_file.sync_all()?;
        drop(staged_file);
        #[cfg(test)]
        before_publish(&stage)?;
        exclusive.verify()?;
        self.identity.verify()?;
        ensure!(
            self.is_ready_disposition(),
            "incompatible_queue: queue startup changed"
        );
        ensure!(
            self.request_file_witness.lock().unwrap().is_none(),
            "incompatible_queue: previously observed requests.db disappeared"
        );
        // rename is safe without a no-clobber primitive because all Trellis
        // publishers and readers take this same index-use lock (T00).
        match fs::symlink_metadata(&path) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        ensure!(
            !queue_sidecar_exists(&stage),
            "storage_busy: staged queue sidecar"
        );
        fs::rename(&stage, &path)?;
        cleanup.published = true;
        // Cross-directory publication must persist removal from the sibling
        // stage directory as well as insertion into the candidate directory.
        fs::File::open(stage.parent().expect("stage has a parent directory"))?.sync_all()?;
        #[cfg(test)]
        after_publish(&path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    fn request_connection(&self) -> Result<(UseGuard, super::ProtectedSqliteConnection)> {
        // A replacement-root caller must never recreate a missing queue
        // that might contain accepted old-root IDs.
        self.request_connection_for_root_loss(false, self.is_root_replaced())
    }
    /// Read an already-admitted queue without creating requests.db or its schema.
    /// A vanished queue we previously observed is not equivalent to a virgin Ready index.
    fn existing_request_connection(
        &self,
    ) -> Result<Option<(UseGuard, super::ProtectedSqliteConnection)>> {
        match self.request_connection_for_root_loss(false, true) {
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
        // Never create the public inode before schema and identity are durable.
        // Drop SH before taking EX: upgrading a live use guard can deadlock.
        if !old_root && !existing_only {
            match fs::symlink_metadata(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    drop(guard);
                    #[cfg(test)]
                    self.bootstrap_missing_queue(|_| Ok(()), |_| Ok(()))?;
                    #[cfg(not(test))]
                    self.bootstrap_missing_queue()?;
                    return self
                        .request_connection_for_root_loss(old_root, true)
                        .map_err(|error| {
                            if error
                                .downcast_ref::<std::io::Error>()
                                .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                            {
                                anyhow::anyhow!(
                                    "incompatible_queue: published requests.db disappeared"
                                )
                            } else {
                                error
                            }
                        });
                }
                Err(error) => return Err(error.into()),
                Ok(_) => {}
            }
        }
        // The protected directory and file must remain private, regular and tied to the pathname.
        let file = super::retained_sqlite_file(&path, true, false, false)?;
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
        let db = super::protected_sqlite_open(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(std::time::Duration::from_secs(3))?;
        db.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON")?;
        // The public queue inode is published only after its complete staged
        // transaction commits. Existing-only and normal callers open it under
        // SH for hot-journal recovery, then validate v1, identity and integrity
        // before interpreting any row; no bootstrap mutates a public v0 inode.
        let version: i64 =
            super::storage_result(db.pragma_query_value(None, "user_version", |r| r.get(0)))?;
        // Even an empty v0 inode may be an accidentally truncated accepted
        // queue. No caller, including a verified writer, can recover it safely.
        ensure!(version == 1, "incompatible_queue: schema version");
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
        // A cross-directory rename may have succeeded while either source
        // or destination sync failed. Persist both directories before the
        // first ACK in this process. SH excludes candidate GC throughout.
        if witness.is_none() {
            let parent = path.parent().expect("queue has a parent directory");
            fs::File::open(parent.parent().expect("index has a parent directory"))?.sync_all()?;
            fs::File::open(parent)?.sync_all()?;
            guard.verify()?;
            let after_sync = fs::symlink_metadata(&path)?;
            ensure!(
                after_sync.is_file()
                    && !after_sync.file_type().is_symlink()
                    && (after_sync.dev(), after_sync.ino()) == current_inode,
                "unsafe requests.db changed during directory sync"
            );
        }
        *witness = Some(current_inode);
        drop(witness);
        if !old_root {
            self.identity.verify()?;
        }
        guard.verify()?;
        Ok((guard, db))
    }
    fn request_connection_for_enqueue_until(
        &self,
        deadline: Instant,
    ) -> Result<(UseGuard, super::ProtectedSqliteConnection)> {
        loop {
            match self.request_connection() {
                Ok(pair) => return Ok(pair),
                Err(error)
                    if error.is::<super::topology::StorageBusy>()
                        || matches!(error.downcast_ref::<rusqlite::Error>(),
                            Some(rusqlite::Error::SqliteFailure(info, _))
                                if matches!(info.code,
                                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)) =>
                {
                    if Instant::now() >= deadline {
                        return Err(super::topology::StorageBusy.into());
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error),
            }
        }
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
        // Retry only queue admission. No request has been inserted yet; once
        // IMMEDIATE begins, even an ambiguous COMMIT must never be re-enqueued.
        let (_guard, mut db) =
            self.request_connection_for_enqueue_until(Instant::now() + Duration::from_secs(3))?;
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
        // The local writer may claim immediately after COMMIT; reading it back would race
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
    /// Inspect a settled foreign-root row without granting it to a new-root client.
    #[cfg(feature = "test-support")]
    pub fn request_any_root_for_tests(&self, id: &str) -> Result<Option<Request>> {
        let Some((_guard, db)) = self.existing_request_connection()? else {
            return Ok(None);
        };
        Ok(db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [id],
                read,
            )
            .optional()?)
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
        let (_guard, db) = match self.request_connection_for_root_loss(true, true) {
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
        self.current_request_with_retry(std::time::Duration::from_secs(3), || {})
    }

    fn current_request_with_retry(
        &self,
        budget: std::time::Duration,
        mut on_busy: impl FnMut(),
    ) -> Result<Option<Request>> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            let attempt = (|| {
                let Some((_guard, db)) = self
                    .existing_request_connection()
                    .map_err(settlement_open_error)?
                else {
                    return Ok(None);
                };
                let row = super::storage_result(db.query_row(
                    &format!("SELECT {COLUMNS} FROM requests WHERE root_device=?1 AND root_inode=?2 ORDER BY seq DESC LIMIT 1"),
                    params![self.identity.device.to_string(), self.identity.inode.to_string()],
                    read,
                ).optional())?;
                self.verify_request_root(&row)?;
                Ok(row)
            })();
            match attempt {
                Err(ref error)
                    if super::transient_storage_contention(error)
                        && std::time::Instant::now() < deadline =>
                {
                    on_busy();
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => return result,
            }
        }
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
    /// Settle a lost captured root without retaining an old writer lease.
    /// A separate protected queue open confirms every terminal row after COMMIT.
    pub fn fail_changed_root_requests(&self) -> Result<usize> {
        ensure!(
            self.identity.root_path_replaced()?,
            "root_changed: captured root is still present"
        );
        let mut seen = Vec::<Request>::new();
        for attempt in 0..2 {
            let result = (|| -> Result<bool> {
                let (_guard, mut db) = self.request_connection_for_root_loss(true, true)?;
                let tx = super::storage_result(
                    db.transaction_with_behavior(TransactionBehavior::Immediate),
                )?;
                ensure!(
                    self.identity.root_path_replaced()?,
                    "root_changed: captured root returned"
                );
                let affected: Vec<Request> = {
                    let mut statement = super::storage_result(tx.prepare(&format!(
                        "SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') AND root_device=?1 AND root_inode=?2 ORDER BY seq"
                    )))?;
                    let rows = super::storage_result(statement.query_map(
                        params![
                            self.identity.device.to_string(),
                            self.identity.inode.to_string()
                        ],
                        read,
                    ))?;
                    super::storage_result(rows.collect::<rusqlite::Result<_>>())?
                };
                for row in &affected {
                    if !seen.iter().any(|prior| prior.seq == row.seq) {
                        seen.push(row.clone());
                    }
                }
                let transition = Uuid::new_v4().to_string();
                let changed = super::storage_result(tx.execute(
                    "UPDATE requests SET state='failed',claim_incarnation=?1,started_at=COALESCE(started_at,?2),finished_at=?2,error_code='root_changed' WHERE state IN ('queued','running') AND root_device=?3 AND root_inode=?4",
                    params![transition, now(), self.identity.device.to_string(), self.identity.inode.to_string()],
                ))?;
                ensure!(
                    changed == affected.len(),
                    "storage_busy: root failure set changed"
                );
                ensure!(
                    self.identity.root_path_replaced()?,
                    "root_changed: captured root returned"
                );
                let committed = super::storage_result(tx.commit());
                drop(db);
                let (_guard, reread) = self.request_connection_for_root_loss(true, true)?;
                let confirmed = attest_prior_root_rows(&reread, &seen)?
                    && self.old_root_pending_in(&reread)? == 0;
                if confirmed {
                    ensure!(
                        self.identity.root_path_replaced()?,
                        "root_changed: captured root returned"
                    );
                    return Ok(true);
                }
                committed?;
                Ok(false)
            })();
            match result {
                Ok(true) => return Ok(seen.len()),
                Ok(false) if attempt == 0 => continue,
                Err(ref error) if attempt == 0 && super::transient_storage_contention(error) => {
                    continue;
                }
                Err(error) => return Err(error),
                Ok(false) => break,
            }
        }
        anyhow::bail!("storage_busy: lost-root settlement not confirmed")
    }

    fn old_root_pending_in(&self, db: &Connection) -> Result<i64> {
        super::storage_result(db.query_row(
            "SELECT count(*) FROM requests WHERE state IN ('queued','running') AND root_device=?1 AND root_inode=?2",
            params![self.identity.device.to_string(), self.identity.inode.to_string()],
            |row| row.get(0),
        ))
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
            // A new daemon creates its queue before first H. After a published
            // head, absence is corruption: accepted IDs could have been lost.
            ensure!(
                self.is_ready_disposition()
                    && self.request_file_witness.lock().unwrap().is_none()
                    && self.index_baseline()?.index_revision == 0,
                "incompatible_queue: accepted requests.db disappeared"
            );
            let (_guard, db) = self.request_connection()?;
            drop(db);
            return Ok(0);
        }
        let mut after_commit = Some(after_commit);
        let mut seen = Vec::<Request>::new();
        // Each protected open has SQLite's three-second busy timeout. Two
        // attempts bound contention; never start H or claim on uncertain rows.
        for attempt_number in 0..2 {
            let attempt = (|| -> Result<bool> {
                let (_guard, mut db) = self
                    .request_connection_for_root_loss(false, true)
                    .map_err(settlement_open_error)?;
                let tx = super::storage_result(
                    db.transaction_with_behavior(TransactionBehavior::Immediate),
                )?;
                self.identity.verify()?;
                let affected: Vec<Request> = {
                    let mut statement = super::storage_result(tx.prepare(&format!(
                        "SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') AND (root_device<>?1 OR root_inode<>?2) ORDER BY seq"
                    )))?;
                    let rows = super::storage_result(statement.query_map(
                        params![
                            self.identity.device.to_string(),
                            self.identity.inode.to_string()
                        ],
                        read,
                    ))?;
                    super::storage_result(rows.collect::<rusqlite::Result<_>>())?
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
                // COMMIT failure is ambiguous. Reopen; this connection's apparent
                // state is not proof of durability.
                drop(db);
                #[cfg(feature = "test-support")]
                let committed = if self.run_ambiguous_settlement_hook_for_tests() {
                    Err(anyhow::anyhow!(
                        "storage_busy: injected ambiguous root settlement COMMIT"
                    ))
                } else {
                    committed
                };
                if committed.is_ok() {
                    let _ = after_commit.take().map_or(Ok(()), |hook| hook());
                }
                let (_guard, reread) = self
                    .request_connection_for_root_loss(false, true)
                    .map_err(settlement_open_error)?;
                let confirmed = attest_foreign_root_settlement(
                    &reread,
                    &affected,
                    &incarnation,
                    &self.identity.device.to_string(),
                    &self.identity.inode.to_string(),
                )? && attest_prior_root_rows(&reread, &seen)?;
                if confirmed {
                    self.identity.verify()?;
                }
                Ok(confirmed)
            })();
            match attempt {
                Ok(true) => return Ok(seen.len()),
                Ok(false) if attempt_number == 0 => continue,
                Err(ref error)
                    if attempt_number == 0 && super::transient_storage_contention(error) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
                Ok(false) => break,
            }
        }
        anyhow::bail!("storage_busy: root settlement not confirmed")
    }

    pub fn claim_request(&self, session: &WriterSession) -> Result<Option<Request>> {
        // EX ownership alone is not authority to claim. The same incarnation
        // must first commit and attest its selected post-acquisition H.
        self.verify_committed_writer_claim(session)?;
        let (_guard, mut db) = self.request_connection()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.verify_writer_session(session)?;
        let row=tx.query_row(&format!("SELECT {COLUMNS} FROM requests WHERE state IN ('queued','running') ORDER BY seq LIMIT 1"), [],read).optional()?;
        let Some(row) = row else { return Ok(None) };
        self.verify_request_root(&Some(row.clone()))?;
        // A running request held by this incarnation belongs to another local driver.
        if row.state == "running"
            && row.claim_incarnation.as_deref() == Some(session.incarnation().to_string().as_str())
        {
            return Ok(None);
        }
        self.verify_committed_writer_claim(session)?;
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
        session: &WriterSession,
        request: &Request,
        before: Option<IndexPin>,
    ) -> Result<()> {
        self.verify_writer_session(session)?;
        self.settle_foreign_root_requests()?;
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
        self.verify_writer_session(session)?;
        self.verify_request_root(&Some(observed))?;
        let changed=tx.execute(
            "UPDATE requests SET state='queued',claim_incarnation=NULL,started_at=NULL WHERE seq=?1 AND id=?2 AND state='running' AND claim_incarnation=?3 AND root_device=?4 AND root_inode=?5",
            params![request.seq,request.id,session.incarnation().to_string(),request.root_device,request.root_inode],
        )?;
        ensure!(
            changed == 1,
            "storage_busy: request claim changed before same-seq requeue"
        );
        self.verify_writer_session(session)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn record_and_finish_request(
        &self,
        session: &WriterSession,
        request: &Request,
        result: Result<IndexPin>,
    ) -> Result<()> {
        self.verify_writer_session(session)?;
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

    /// Wake one in-process test after this exact accepted ID commits terminally.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn notify_on_terminal_request_for_tests(&self, id: &str, notify: Arc<tokio::sync::Notify>) {
        *self.test_queue_terminal_notify.lock().unwrap() = Some(TestTerminalNotify {
            id: id.to_owned(),
            notify,
        });
    }

    /// Read a timeout diagnostic without blocking behind a terminal queue write.
    #[cfg(test)]
    pub(crate) fn test_pending_completion_probe_for_tests(
        &self,
    ) -> (bool, Option<(String, CompletionOutcome)>) {
        match self.pending_request_completion.try_lock() {
            Ok(slot) => (
                false,
                slot.as_ref()
                    .map(|pending| (pending.request.id.clone(), pending.outcome.clone())),
            ),
            Err(_) => (true, None),
        }
    }

    /// A daemon caller may retry only its own cached completion under the same writer.
    /// Do not convert an ordinary claim/read/publish error into an unbounded retry.
    pub(crate) fn has_recorded_completion(&self, session: &WriterSession) -> Result<bool> {
        Ok(self
            .pending_request_completion
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|pending| pending.incarnation == session.incarnation().to_string()))
    }

    /// Resolve the one in-flight FIFO head before claiming any newer row. A terminal
    /// reread handles an ambiguous SQLite COMMIT; it must match our cached result.
    pub(crate) fn retry_recorded_completion(&self, session: &WriterSession) -> Result<bool> {
        self.verify_writer_session(session)?;
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
        session: &WriterSession,
        request: &Request,
        result: Result<IndexPin>,
    ) -> Result<()> {
        self.finish_request_outcome(session, request, &CompletionOutcome::from_result(result)?)
    }
    fn finish_request_outcome(
        &self,
        session: &WriterSession,
        request: &Request,
        outcome: &CompletionOutcome,
    ) -> Result<()> {
        self.verify_writer_session(session)?;
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
        self.verify_writer_session(session)?;
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
        #[cfg(any(test, feature = "test-support"))]
        {
            let notify = {
                let mut hook = self.test_queue_terminal_notify.lock().unwrap();
                if hook.as_ref().is_some_and(|target| target.id == request.id) {
                    hook.take().map(|target| target.notify)
                } else {
                    None
                }
            };
            if let Some(notify) = notify {
                notify.notify_one();
            }
        }
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
    fn locked_post_commit_reopen_returns_typed_busy_without_losing_terminal_row() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let accepted = store
            .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
            .unwrap();
        let path = store.request_db_path();
        let setup = Connection::open(&path).unwrap();
        setup
            .execute(
                "UPDATE requests SET root_inode='old' WHERE id=?1",
                [&accepted.id],
            )
            .unwrap();
        drop(setup);
        let blocker = Connection::open(&path).unwrap();
        let error = store
            .settle_foreign_root_requests_with_hook(|| {
                blocker.execute_batch("BEGIN EXCLUSIVE")?;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "storage_busy: SQLite lock contention");
        assert!(
            error
                .downcast_ref::<super::super::SqliteContention>()
                .is_some()
        );
        assert!(
            !error
                .chain()
                .any(|cause| cause.downcast_ref::<rusqlite::Error>().is_some())
        );
        blocker.execute_batch("ROLLBACK").unwrap();
        assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
        let db = Connection::open(&path).unwrap();
        let terminal = db
            .query_row(
                &format!("SELECT {COLUMNS} FROM requests WHERE id=?1"),
                [&accepted.id],
                read,
            )
            .unwrap();
        assert_eq!(
            (
                terminal.id,
                terminal.seq,
                terminal.state.as_str(),
                terminal.error_code.as_deref()
            ),
            (accepted.id, accepted.seq, "failed", Some("root_changed"))
        );
    }

    #[test]
    fn current_request_rejects_existing_v0_without_creating_schema() {
        use std::os::unix::fs::OpenOptionsExt;
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let path = store.request_db_path();
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let mut attempts = 0;
        let error = store
            .current_request_with_retry(std::time::Duration::from_secs(3), || {
                attempts += 1;
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "incompatible_queue: schema version");
        assert_eq!(attempts, 0);
    }

    #[test]
    fn current_request_v0_is_incompatible_even_at_retry_deadline() {
        use std::os::unix::fs::OpenOptionsExt;
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let path = store.request_db_path();
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let error = store
            .current_request_with_retry(std::time::Duration::ZERO, || {
                panic!("no retry after deadline")
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "incompatible_queue: schema version");
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            0
        );
        let objects: i64 = db
            .query_row("SELECT count(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        assert_eq!(objects, 0);
    }

    #[test]
    fn private_stage_fault_before_publish_leaves_main_absent_and_retries() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let path = store.request_db_path();
        let mut stage_path = None;
        let failure = store
            .bootstrap_missing_queue(
                |stage| {
                    let db = Connection::open_with_flags(
                        stage,
                        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                    )?;
                    assert_eq!(
                        db.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))?,
                        1
                    );
                    assert!(!path.exists());
                    stage_path = Some(stage.to_owned());
                    anyhow::bail!("injected error before queue publication")
                },
                |_| Ok(()),
            )
            .unwrap_err();
        assert_eq!(
            failure.to_string(),
            "injected error before queue publication"
        );
        let stage = stage_path.unwrap();
        assert!(
            !stage.exists(),
            "ordinary errors remove their private stage"
        );
        assert!(!path.exists());
        assert!(
            fs::read_dir(path.parent().unwrap()).unwrap().all(|entry| {
                matches!(
                    entry.unwrap().file_name().to_str(),
                    Some("index.db" | "requests.db")
                )
            }),
            "failed stage must not change candidate GC inventory"
        );
        assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
        assert!(path.exists());
    }

    #[test]
    fn crashed_stage_orphan_sibling_does_not_change_candidate_gc_inventory() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let queue = store.request_db_path();
        let mut orphan = None;
        let _ = store.bootstrap_missing_queue(
            |stage| {
                let sibling = stage.with_extension("crash-orphan");
                fs::copy(stage, &sibling)?;
                orphan = Some(sibling);
                anyhow::bail!("simulate orphan from process death")
            },
            |_| Ok(()),
        );
        let orphan = orphan.unwrap();
        assert!(orphan.exists());
        assert_eq!(orphan.parent(), queue.parent().unwrap().parent());
        assert!(!queue.exists());
        assert!(fs::read_dir(queue.parent().unwrap()).unwrap().all(|entry| {
            matches!(
                entry.unwrap().file_name().to_str(),
                Some("index.db" | "requests.db")
            )
        }));
        assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
    }

    #[test]
    fn publish_between_dir_syncs_reopens_v1_without_losing_witness_gate() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let queue = store.request_db_path();
        let error = store
            .bootstrap_missing_queue(
                |_| Ok(()),
                |published| {
                    assert_eq!(published, queue);
                    anyhow::bail!("injected failure between source and destination sync")
                },
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "injected failure between source and destination sync"
        );
        assert!(queue.exists(), "published v1 must not be discarded");
        assert!(store.request_file_witness.lock().unwrap().is_none());
        // Existing-only admission validates v1 and syncs both source and
        // destination directories before setting its first process witness.
        assert!(store.existing_request_connection().unwrap().is_some());
        assert!(store.request_file_witness.lock().unwrap().is_some());
        assert!(store.current_request().unwrap().is_none());
    }

    #[test]
    fn published_stage_reopens_with_committed_identity() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        store
            .bootstrap_missing_queue(|_| Ok(()), |_| Ok(()))
            .unwrap();
        let path = store.request_db_path();
        let expected = (
            store.workspace_root.clone(),
            store.identity.root_key.clone(),
        );
        drop(store);
        let db =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let identity: (String, String) = db
            .query_row(
                "SELECT root_spelling,root_key FROM queue_identity",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(identity, expected);
        drop(db);
        let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert!(reopened.current_request().unwrap().is_none());
    }

    #[test]
    fn accepted_before_h_accidentally_truncated_v0_is_never_initialized() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let accepted = store
            .enqueue_request(&IndexOptions::new(root.path().to_owned()), None)
            .unwrap();
        assert!(!accepted.id.is_empty());
        let path = store.request_db_path();
        drop(store);
        fs::write(&path, []).unwrap();
        let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
        let error = reopened.settle_foreign_root_requests().unwrap_err();
        assert!(
            error.to_string().contains("incompatible_queue"),
            "{error:#}"
        );
        drop(reopened);
        let db = Connection::open(&path).unwrap();
        assert_eq!(
            db.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn concurrent_creator_cannot_publish_over_first_queue() {
        use std::sync::mpsc;
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let second = store.clone();
        let (staged_tx, staged_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            second.bootstrap_missing_queue(
                |_| {
                    staged_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                },
                |_| Ok(()),
            )
        });
        staged_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let main_absent = !store.request_db_path().exists();
        let contender = store.bootstrap_missing_queue(|_| Ok(()), |_| Ok(()));
        resume_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
        assert!(main_absent);
        assert!(
            contender
                .unwrap_err()
                .is::<super::super::topology::StorageBusy>()
        );
        let inode = fs::metadata(store.request_db_path()).unwrap().ino();
        store
            .bootstrap_missing_queue(|_| panic!("loser must reopen winner"), |_| Ok(()))
            .unwrap();
        assert_eq!(fs::metadata(store.request_db_path()).unwrap().ino(), inode);
        assert!(store.current_request().unwrap().is_none());
    }

    #[test]
    fn enqueue_waits_for_other_store_bootstrap_before_single_accept() {
        use std::sync::mpsc;
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let builder = Store::open_for_tests(state.path(), root.path()).unwrap();
        let caller = Store::open_for_tests(state.path(), root.path()).unwrap();
        let (staged_tx, staged_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let creator = std::thread::spawn(move || {
            builder.bootstrap_missing_queue(
                |_| {
                    staged_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                },
                |_| Ok(()),
            )
        });
        staged_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let (retry_tx, retry_rx) = mpsc::channel();
        let hook = caller.test_queue_before_shared_hook.clone();
        caller.test_queue_before_shared_hook.set(move || {
            hook.set(move || retry_tx.send(()).unwrap());
        });
        let options = IndexOptions::new(root.path().to_owned());
        let queued = std::thread::spawn(move || caller.enqueue_request(&options, None));
        // The second SH attempt proves the first one failed under builder EX.
        let reached_retry = retry_rx.recv_timeout(Duration::from_secs(5));
        // Release EX and join even when the expected retry signal was missed.
        resume_tx.send(()).unwrap();
        creator.join().unwrap().unwrap();
        let accepted = queued.join().unwrap().unwrap();
        reached_retry.unwrap();
        let reopened = Store::open_for_tests(state.path(), root.path()).unwrap();
        let (_guard, db) = reopened.request_connection().unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(
            reopened.request_by_id(&accepted.id).unwrap().unwrap().id,
            accepted.id
        );
    }

    #[test]
    fn exhausted_preaccept_admission_returns_busy_without_row() {
        use std::sync::mpsc;
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let builder = Store::open_for_tests(state.path(), root.path()).unwrap();
        let caller = Store::open_for_tests(state.path(), root.path()).unwrap();
        let (staged_tx, staged_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let creator = std::thread::spawn(move || {
            builder.bootstrap_missing_queue(
                |_| {
                    staged_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                },
                |_| Ok(()),
            )
        });
        staged_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let error = caller
            .request_connection_for_enqueue_until(Instant::now())
            .err()
            .unwrap();
        assert!(error.is::<super::super::topology::StorageBusy>());
        assert!(!caller.request_db_path().exists());
        resume_tx.send(()).unwrap();
        creator.join().unwrap().unwrap();
        let (_guard, db) = caller.request_connection().unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn root_replaced_missing_queue_does_not_publish() {
        let state = tempfile::tempdir().unwrap();
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("workspace");
        fs::create_dir(&root).unwrap();
        let store = Store::open_for_tests(state.path(), &root).unwrap();
        fs::rename(&root, parent.path().join("old-workspace")).unwrap();
        fs::create_dir(&root).unwrap();
        assert!(store.request_connection().is_err());
        assert!(!store.request_db_path().exists());
    }

    #[test]
    fn missing_virgin_queue_bootstraps_empty_queue_before_first_h() {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        assert!(!store.request_db_path().exists());
        assert_eq!(store.settle_foreign_root_requests().unwrap(), 0);
        assert!(store.request_db_path().exists());
        assert!(store.current_request().unwrap().is_none());
    }
}
