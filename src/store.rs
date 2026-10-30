//! SQLite index snapshots and request queues. Connections are never shared between threads.
pub mod anchors;
mod graph_read;
pub(crate) mod id_codec;
pub(crate) mod mcp_read;
mod native_join_budget;
mod published_admission;
pub mod requests;
mod search;
mod search_startup;
pub(crate) use search::SEARCH_CARD_BATCH_SIZE;
pub use search::{
    HeadSearchCard, HeadSearchCards, McpSearchCandidates, SearchDeltaColdRebuild,
    SearchHeadChanged, SearchHeadDelta,
};
pub use search_startup::RecentSearchCheckout;
pub(crate) mod symbol_trigrams;
pub mod topology;
use crate::model::*;
use crate::native_evidence::DeclarationRole;
use anyhow::{Context, Result, ensure};
pub use requests::{MaintenanceQueueProbe, MaintenanceQueueState, QueueProbeAdmission};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ops::{Deref, DerefMut},
    path::Path,
    sync::{
        Arc, Condvar, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(test)]
type TestMetricsDocumentHook = Arc<dyn Fn(&str, MetricsDocumentStage) + Send + Sync>;
#[cfg(test)]
type TestStreamPreflightHook = Box<dyn FnOnce(&crate::native_evidence::FullDocumentSpool) + Send>;
/// When a metrics document's test hook runs.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetricsDocumentStage {
    /// Its metrics work is starting.
    Start,
    /// That work has returned, with success or failure.
    Done,
}
#[derive(Default)]
pub(crate) struct TestOneShotHook(Mutex<Option<Box<dyn FnOnce() + Send>>>);
impl std::fmt::Debug for TestOneShotHook {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TestOneShotHook")
    }
}
impl TestOneShotHook {
    pub(crate) fn set(&self, hook: impl FnOnce() + Send + 'static) {
        *self.0.lock().unwrap() = Some(Box::new(hook));
    }
    pub(crate) fn run(&self) {
        let hook = self.0.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

#[derive(Debug)]
struct RetentionClock {
    origin_wall: i64,
    origin_mono: u64,
    started: Instant,
    injected: Option<(i64, u64)>,
}
impl RetentionClock {
    fn sample(&self) -> Result<(i64, u64)> {
        match self.injected {
            Some(pair) => Ok(pair),
            None => Ok((
                publication_second()?,
                self.origin_mono
                    .saturating_add(self.started.elapsed().as_secs()),
            )),
        }
    }
}

// A single process/root gate serializes maintenance with the entire publication
// lifetime, including terminal ACK. Independent Store instances share the gate,
// and through it the queue's in-process signals (Decision 0009 §8).
#[derive(Default)]
struct PublicationGate {
    state: Mutex<GateState>,
    changed: Condvar,
    /// Checkout workers to wake after any Store instance commits an enqueue.
    queue_wakers: Mutex<Vec<std::sync::Weak<dyn Fn() + Send + Sync>>>,
    /// Enqueues between their start and their wake, plus `CLOSING` while a
    /// runtime for this queue is closing (Decision 0009 §5). Retirement sets
    /// `CLOSING` only if no enqueue is in flight, in one compare-and-swap:
    /// every enqueue either counts before that instant (and the retirement
    /// cancels) or starts after it (and waits for the next activation).
    enqueue_word: std::sync::atomic::AtomicUsize,
    #[cfg(feature = "test-support")]
    test_after_enqueue_commit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// `state.waiting`, published for lock-free maintenance checks (#138
    /// item 12): a briefly held state lock never reads as a waiting publisher.
    waiting_published: std::sync::atomic::AtomicUsize,
    /// Wake generation after a confirmed terminal request COMMIT or loss of
    /// the queue's index directory, by any Store instance (Decision 0009 §8).
    /// A waiter re-reads its row after a wake; requests.db alone records outcomes.
    terminal_commits: Mutex<u64>,
    #[cfg(feature = "test-support")]
    test_before_terminal_wait: TestOneShotHook,
    terminal_changed: Condvar,
    /// Set while a live publication's transaction has spilled pages to
    /// `index.db`. In rollback-journal mode that spill takes EXCLUSIVE until
    /// COMMIT, so every reader of the prior head is locked out (#210).
    readers_blocked: Mutex<Option<RebuildInProgress>>,
    #[cfg(test)]
    test_after_readers_unblocked: TestOneShotHook,
    #[cfg(feature = "test-support")]
    test_ambiguous_settlement_once: AtomicBool,
    #[cfg(feature = "test-support")]
    test_before_settlement_confirmation: TestOneShotHook,
}
#[derive(Debug, Default)]
struct GateState {
    active: bool,
    maintenance: bool,
    waiting: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicationWaitReason {
    None,
    Maintenance,
    Publisher,
}
impl std::fmt::Debug for PublicationGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PublicationGate").finish_non_exhaustive()
    }
}
impl PublicationGate {
    fn publish_waiting(&self, state: &GateState) {
        self.waiting_published
            .store(state.waiting, std::sync::atomic::Ordering::Release);
    }

    fn notify_terminal(&self) {
        *self.terminal_commits.lock().unwrap() += 1;
        self.terminal_changed.notify_all();
    }

    fn wake_queue(&self) {
        let wakers: Vec<_> = {
            let mut wakers = self.queue_wakers.lock().unwrap();
            wakers.retain(|waker| waker.strong_count() > 0);
            wakers.iter().filter_map(std::sync::Weak::upgrade).collect()
        };
        for wake in wakers {
            wake();
        }
    }
}
/// The closing bit of a queue's enqueue word; the rest counts enqueues.
const CLOSING: usize = 1 << (usize::BITS - 1);

/// A read found the index locked by a publication that has spilled to the
/// file, which holds readers out until it commits (#210). Readers fail with
/// this at once instead of waiting out their busy budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildInProgress {
    pub since: SystemTime,
    pub documents: usize,
}
impl std::fmt::Display for RebuildInProgress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let held = SystemTime::now()
            .duration_since(self.since)
            .unwrap_or_default()
            .as_secs();
        write!(
            f,
            "rebuild_in_progress: a publication of {} documents has locked the index for {held} s; retry after it commits",
            self.documents
        )
    }
}
impl std::error::Error for RebuildInProgress {}

thread_local! {
    /// The publication gate of the index this thread last opened for reading.
    /// SQLite's busy handler is a plain function, so it finds the gate here.
    static READ_GATE: std::cell::RefCell<Weak<PublicationGate>> = const { std::cell::RefCell::new(Weak::new()) };
}

/// Whether the index this thread reads is locked by a spilled publication.
fn read_gate_blocked() -> Option<RebuildInProgress> {
    READ_GATE
        .with(|gate| gate.borrow().upgrade())
        .and_then(|gate| gate.readers_blocked.lock().unwrap().clone())
}

/// Marks a live publication's transaction as blocking readers once SQLite
/// has spilled its dirty pages to the file, and clears the mark when the
/// transaction ends by commit, rollback, error or unwind.
struct ReadersBlockedWatch<'a> {
    gate: &'a PublicationGate,
    documents: usize,
    marked: bool,
}
impl<'a> ReadersBlockedWatch<'a> {
    fn new(gate: &'a PublicationGate, documents: usize) -> Self {
        Self {
            gate,
            documents,
            marked: false,
        }
    }
    fn check(&mut self, db: &Connection) {
        if self.marked {
            return;
        }
        let (mut current, mut highwater) = (0, 0);
        // SAFETY: the handle belongs to a live connection used on this thread,
        // and the out-pointers are valid locals.
        let status = unsafe {
            rusqlite::ffi::sqlite3_db_status(
                db.handle(),
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_SPILL,
                &mut current,
                &mut highwater,
                0,
            )
        };
        if status == rusqlite::ffi::SQLITE_OK && current > 0 {
            *self.gate.readers_blocked.lock().unwrap() = Some(RebuildInProgress {
                since: SystemTime::now(),
                documents: self.documents,
            });
            self.marked = true;
        }
    }
}
impl Drop for ReadersBlockedWatch<'_> {
    fn drop(&mut self) {
        if self.marked {
            *self.gate.readers_blocked.lock().unwrap() = None;
            #[cfg(test)]
            self.gate.test_after_readers_unblocked.run();
        }
    }
}

/// An enqueue counted as in flight on its queue's gate.
pub(crate) struct EnqueueInFlight(Arc<PublicationGate>);
impl Drop for EnqueueInFlight {
    fn drop(&mut self) {
        self.0
            .enqueue_word
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}
#[must_use]
#[derive(Debug)]
pub struct PublishPermit {
    gate: Arc<PublicationGate>,
    waited_for: Duration,
    wait_reason: PublicationWaitReason,
}
impl PublishPermit {
    pub fn waited_for(&self) -> Duration {
        self.waited_for
    }
    pub fn wait_reason(&self) -> PublicationWaitReason {
        self.wait_reason
    }
}
#[must_use]
#[derive(Debug)]
pub struct MaintenancePermit {
    gate: Arc<PublicationGate>,
}
impl Drop for PublishPermit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap();
        state.active = false;
        state.maintenance = false;
        self.gate.changed.notify_all();
    }
}
impl Drop for MaintenancePermit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().unwrap();
        state.active = false;
        state.maintenance = false;
        self.gate.changed.notify_all();
    }
}
fn publication_gate(key: String) -> Arc<PublicationGate> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<PublicationGate>>>> = OnceLock::new();
    let mut gates = GATES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap();
    if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
        return gate;
    }
    gates.retain(|_, weak| weak.strong_count() > 0);
    let gate = Arc::new(PublicationGate::default());
    gates.insert(key, Arc::downgrade(&gate));
    gate
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceOutcome {
    Idle,
    Deferred,
    Progress,
}

#[cfg(any(test, feature = "test-support"))]
struct TestTerminalNotify {
    id: String,
    notify: Arc<tokio::sync::Notify>,
}

#[cfg(feature = "test-support")]
pub type RetentionStateForTests = ((String, i64, i64), (i64, i64));

#[cfg(feature = "test-support")]
#[derive(Debug)]
#[doc(hidden)]
pub struct PrivateStageObservation {
    pub pin: IndexPin,
    pub semantic: crate::semantic_evidence::SemanticSnapshot,
    pub native_revision: crate::native_evidence::Revision,
    pub declarations: Vec<crate::native_evidence::Declaration>,
    pub calls: Vec<crate::native_evidence::Call>,
}

/// A retained tuple is presented at its own native revision. Freshness is a
/// separate comparison with the requested pin; captured provenance is unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticHistory {
    pub coverage: crate::semantic_evidence::Coverage,
    pub provenance: Vec<crate::semantic_evidence::Provenance>,
    pub presentation_freshness: crate::semantic_evidence::Freshness,
}

#[derive(Clone)]
pub struct Store {
    roots: topology::TopologyRoots,
    identity: Arc<topology::WorkspaceIdentity>,
    publication_gate: Arc<PublicationGate>,
    maintenance_busy_attempts: Arc<AtomicU64>,
    #[cfg(feature = "test-support")]
    mcp_read_busy_attempts: Arc<std::sync::atomic::AtomicUsize>,
    workspace_root: String,
    recovery_required: Arc<AtomicBool>,
    recovery_disposition: Arc<AtomicU8>,
    obsolete_format_marker: Arc<Mutex<Option<ObsoleteIndexFormat>>>,
    pending_request_completion: Arc<Mutex<Option<requests::PendingCompletion>>>,
    request_file_witness: Arc<Mutex<Option<(u64, u64)>>>,
    aborted_staged_index: Arc<Mutex<Option<StagedIndex>>>,
    writer_counters: Arc<Mutex<Option<WriterCounters>>>,
    // This Store's leader may claim FIFO only after its own post-acquisition
    // selected reconciliation was observed committed under the synced EX.
    reconciled_writer: Arc<Mutex<Option<uuid::Uuid>>>,
    retention_clock: Arc<Mutex<RetentionClock>>,
    metrics_memo: Arc<Mutex<crate::metrics::MetricsMemo>>,
    /// Set only on an `EvidenceResponse`'s own Store, which is used only with
    /// that response's read snapshot: the pin it admitted and that revision's
    /// header values. A later read of the same pin in the response shares
    /// them after repeating its own status and pin checks.
    admitted: Option<(IndexPin, Arc<SelectedHeaders>)>,
    published_admission: Arc<published_admission::PublishedAdmission>,
    #[cfg(feature = "test-support")]
    test_after_claim_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_before_request_terminal_commit_hook: Arc<TestOneShotHook>,
    maintenance_before_writer_hook: Arc<TestOneShotHook>,
    maintenance_after_first_delete_hook: Arc<TestOneShotHook>,
    publication_before_commit_hook: Arc<TestOneShotHook>,
    #[cfg(feature = "test-support")]
    publication_before_semantic_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_stream_before_preflight_hook: Arc<Mutex<Option<TestStreamPreflightHook>>>,
    /// The publication page cache in KiB; only tests lower it.
    publication_cache_kib: Arc<std::sync::atomic::AtomicI64>,
    writer_before_metadata_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_queue_before_shared_hook: Arc<TestOneShotHook>,
    /// Called with each metrics document's path as its metrics work starts.
    #[cfg(test)]
    test_metrics_document_hook: Arc<Mutex<Option<TestMetricsDocumentHook>>>,
    #[cfg(feature = "test-support")]
    test_queue_after_root_verify_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_queue_select_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_native_spool_hook: Arc<TestOneShotHook>,
    #[cfg(any(test, feature = "test-support"))]
    test_queue_terminal_notify: Arc<Mutex<Option<TestTerminalNotify>>>,
    #[cfg(feature = "test-support")]
    test_ambiguous_completion_once: Arc<AtomicBool>,
    #[cfg(test)]
    test_exclusive_recovery_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_queue_finish_failures: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    test_queue_post_commit_failures: Arc<std::sync::atomic::AtomicUsize>,
    #[cfg(test)]
    test_publish_commit_busy_once: Arc<AtomicBool>,
    // Distinct direct typed SQLite BUSY fixture for mandatory startup reconciliation retries.
    // The older plain-string hook remains unchanged for classifier tests.
    #[cfg(test)]
    test_publish_commit_typed_busy_once: Arc<AtomicBool>,
    #[cfg(test)]
    test_publish_post_commit_busy_once: Arc<AtomicBool>,
}
impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Store")
            .field("workspace_root", &self.workspace_root)
            .finish_non_exhaustive()
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryDisposition {
    Ready = 0,
    Rebuild = 1,
    RecreatePending = 2,
    RootReplaced = 3,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataColumn {
    SchemaVersion,
    ExtractorVersion,
    IndexGeneration,
    IndexRevision,
}
impl MetadataColumn {
    fn sql(self) -> (&'static str, &'static str, &'static str, &'static [u8]) {
        match self {
            Self::SchemaVersion => (
                "SELECT typeof(schema_version) FROM index_metadata WHERE singleton=1",
                "SELECT schema_version FROM index_metadata WHERE singleton=1",
                "schema_version",
                b"schema_version",
            ),
            Self::ExtractorVersion => (
                "SELECT typeof(extractor_version) FROM index_metadata WHERE singleton=1",
                "SELECT extractor_version FROM index_metadata WHERE singleton=1",
                "extractor_version",
                b"extractor_version",
            ),
            Self::IndexGeneration => (
                "SELECT typeof(index_generation) FROM index_metadata WHERE singleton=1",
                "SELECT index_generation FROM index_metadata WHERE singleton=1",
                "index_generation",
                b"index_generation",
            ),
            Self::IndexRevision => (
                "SELECT typeof(index_revision) FROM index_metadata WHERE singleton=1",
                "SELECT index_revision FROM index_metadata WHERE singleton=1",
                "index_revision",
                b"index_revision",
            ),
        }
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
enum MetadataAtom {
    Null,
    Integer(i64),
    Real(u64),
    Streamed {
        column: MetadataColumn,
        text: bool,
        byte_length: usize,
        sha256: [u8; 32],
    },
}
#[derive(Clone, Debug, Eq, PartialEq)]
struct RecoveryWitness {
    pragma_schema: u32,
    schema_version: MetadataAtom,
    extractor_version: MetadataAtom,
    index_generation: MetadataAtom,
    index_revision: MetadataAtom,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryBaseline {
    witness: RecoveryWitness,
    pin: Option<IndexPin>,
    compatible: bool,
}
impl RecoveryBaseline {
    pub(crate) fn pin(&self) -> Option<IndexPin> {
        self.pin
    }
}
#[derive(Clone, Debug)]
enum ExpectedPublication {
    Pin(IndexPin),
    Recovery(Box<RecoveryBaseline>),
}
impl ExpectedPublication {
    fn pin(&self) -> Option<IndexPin> {
        match self {
            Self::Pin(pin) => Some(*pin),
            Self::Recovery(baseline) => baseline.pin(),
        }
    }
}
#[derive(Clone, Copy)]
enum PublicationTarget<'a> {
    Live,
    Stage(&'a StagedIndex),
}
struct PublicationPlan<'a> {
    expected: ExpectedPublication,
    target: PublicationTarget<'a>,
}
#[derive(Debug)]
struct ForeignStagedIndex;
impl std::fmt::Display for ForeignStagedIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "unsafe_index: failed stage pathname occupied by a foreign inode; witness retained",
        )
    }
}
impl std::error::Error for ForeignStagedIndex {}
#[derive(Debug)]
struct ExceptionalIndexFormat;
impl std::fmt::Display for ExceptionalIndexFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("exceptional index format recovery deferred")
    }
}
impl std::error::Error for ExceptionalIndexFormat {}
#[derive(Clone, Debug, Eq, PartialEq)]
struct IndexFormatMarker {
    schema_version: i64,
    extractor_version: String,
}
impl IndexFormatMarker {
    fn is_obsolete(&self) -> bool {
        self.schema_version != i64::from(DATABASE_SCHEMA_VERSION)
            || self.extractor_version != EXTRACTOR_VERSION
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
enum ObsoleteIndexFormat {
    EarlierSchema(u32),
    Metadata(IndexFormatMarker),
}
impl std::fmt::Display for ObsoleteIndexFormat {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("recovery_required: obsolete index format")
    }
}
impl std::error::Error for ObsoleteIndexFormat {}
#[derive(Debug)]
pub struct PinExpired;
impl std::fmt::Display for PinExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("pin_expired: released native revision")
    }
}
impl std::error::Error for PinExpired {}
#[derive(Debug)]
pub(crate) struct SelectedIntegrity(pub(crate) String);
impl std::fmt::Display for SelectedIntegrity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for SelectedIntegrity {}
#[derive(Debug)]
struct ControlIntegrity(String);
impl std::fmt::Display for ControlIntegrity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}
impl std::error::Error for ControlIntegrity {}
macro_rules! selected_ensure {
    ($condition:expr, $($message:tt)+) => {
        if !$condition {
            return Err(SelectedIntegrity(format!($($message)+)).into());
        }
    };
}
macro_rules! control_ensure {
    ($condition:expr, $message:expr $(,)?) => {
        if !$condition {
            return Err(ControlIntegrity($message.into()).into());
        }
    };
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PublishStage {
    BeforeTransaction,
    AfterFile,
    BeforeCommit,
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum ActivationStage {
    BeforeRename,
    AfterJournalBackupBeforeDirFsync,
    AfterRenameBeforeDirFsync,
}
// Serde's compact JSON serializer writes exactly the bytes persisted by `json()`.
// Count and abort while streaming; never build an over-limit encoded source.
struct EncodedSourceBudget {
    bytes: usize,
    max_bytes: usize,
}
impl std::io::Write for EncodedSourceBudget {
    fn write(&mut self, chunk: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(chunk.len())
            .filter(|size| *size <= self.max_bytes)
            .ok_or_else(|| std::io::Error::other("selected source JSON budget exceeded"))?;
        self.bytes = next;
        Ok(chunk.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
const SELECTED_ANCILLARY_BYTE_SQL: &str = r#"SELECT COALESCE(max(row_bytes),0),COALESCE(sum(row_bytes),0) FROM (
SELECT ?3+COALESCE(length(CAST(x.role_kind AS BLOB)),0)+COALESCE(length(CAST(x.role AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0) AS row_bytes FROM native_version_coverage_roles x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.syntax_id IS NULL THEN 0 ELSE 39 END)+(CASE WHEN x.owner_syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.kind AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.lookup_key AS BLOB)),0)+COALESCE(length(CAST(x.key_signature_present AS BLOB)),0)+COALESCE(length(CAST(x.key_type_parameter_count AS BLOB)),0)+COALESCE(length(CAST(x.key_variadic AS BLOB)),0)+COALESCE(length(CAST(x.key_ordinal AS BLOB)),0)+COALESCE(length(CAST(x.start_byte AS BLOB)),0)+COALESCE(length(CAST(x.end_byte AS BLOB)),0)+COALESCE(length(CAST(x.name_start AS BLOB)),0)+COALESCE(length(CAST(x.name_end AS BLOB)),0) AS row_bytes FROM native_version_declarations x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.kind AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.sibling_ordinal AS BLOB)),0)+COALESCE(length(CAST(x.signature_present AS BLOB)),0)+COALESCE(length(CAST(x.type_parameter_count AS BLOB)),0)+COALESCE(length(CAST(x.variadic AS BLOB)),0) AS row_bytes FROM native_version_declaration_ancestors x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.type_name AS BLOB)),0) AS row_bytes FROM native_version_own_signature_types x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.ancestor_ordinal AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.type_name AS BLOB)),0) AS row_bytes FROM native_version_ancestor_signature_types x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.kind AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.result_type AS BLOB)),0) AS row_bytes FROM native_version_headers x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.item_kind AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.value AS BLOB)),0) AS row_bytes FROM native_version_header_items x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.type_name AS BLOB)),0)+COALESCE(length(CAST(x.variadic AS BLOB)),0) AS row_bytes FROM native_version_parameters x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.id IS NULL THEN 0 ELSE 39 END)+(CASE WHEN x.owner_syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.start_byte AS BLOB)),0)+COALESCE(length(CAST(x.end_byte AS BLOB)),0)+COALESCE(length(CAST(x.callee_start AS BLOB)),0)+COALESCE(length(CAST(x.callee_end AS BLOB)),0)+COALESCE(length(CAST(x.spelling AS BLOB)),0)+?4+COALESCE(length(CAST(x.language AS BLOB)),0)+COALESCE(length(CAST(x.callee_lookup_key AS BLOB)),0) AS row_bytes FROM native_version_calls x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.id IS NULL THEN 0 ELSE 39 END)+(CASE WHEN x.owner_syntax_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.kind AS BLOB)),0)+COALESCE(length(CAST(x.start_byte AS BLOB)),0)+COALESCE(length(CAST(x.end_byte AS BLOB)),0)+(CASE WHEN x.parent_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.arm AS BLOB)),0) AS row_bytes FROM native_version_control_regions x WHERE x.vid=?1
UNION ALL
SELECT ?3+(CASE WHEN x.call_id IS NULL THEN 0 ELSE 39 END)+(CASE WHEN x.region_id IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.ordinal AS BLOB)),0) AS row_bytes FROM native_version_call_regions x WHERE x.vid=?1
UNION ALL
SELECT ?3+78+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.start_byte AS BLOB)),0)+COALESCE(length(CAST(x.end_byte AS BLOB)),0)+length(CAST(x.spelling AS BLOB))+length(CAST(COALESCE(x.lookup_key,x.spelling) AS BLOB)) AS row_bytes FROM native_version_references x WHERE x.vid=?1
UNION ALL
SELECT ?3+length(CAST(x.kind AS BLOB))+length(CAST(x.available AS BLOB))+COALESCE(length(CAST(x.diagnostic AS BLOB)),0)+length(CAST(x.ordinal AS BLOB)) AS row_bytes FROM native_version_capability_families x WHERE x.vid=?1
UNION ALL
SELECT ?3+length(CAST(x.kind AS BLOB))+length(CAST(x.ordinal AS BLOB))+length(CAST(x.start_byte AS BLOB))+length(CAST(x.end_byte AS BLOB))+length(CAST(x.diagnostic AS BLOB)) AS row_bytes FROM native_version_capability_gaps x WHERE x.vid=?1
UNION ALL
SELECT (SELECT length(content_hash)+20 FROM class_projections WHERE cid=x.cid)+(SELECT length(graph_hash)+9 FROM graph_projections WHERE gid=x.gid)+39+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.qualified_name AS BLOB)),0)+COALESCE(length(CAST(x.path AS BLOB)),0)+COALESCE(length(CAST(x.payload AS BLOB)),0) AS row_bytes FROM classes x WHERE x.cid=?2
UNION ALL
SELECT (SELECT length(content_hash)+20 FROM class_projections WHERE cid=x.cid)+COALESCE(length(CAST(x.id AS BLOB)),0)+39+(CASE WHEN x.target IS NULL THEN 0 ELSE 39 END)+COALESCE(length(CAST(x.payload AS BLOB)),0) AS row_bytes FROM class_relations x WHERE x.cid=?2
)"#;
const DATABASE_SCHEMA_VERSION: u32 = 29;
const EXTRACTOR_VERSION: &str = "native-v6-delta-v1";
const EVIDENCE_FORMAT: &str = "terminal-native-graph-v1";
const CACHE_SCHEMA_V29: &str = r#"
CREATE TABLE index_metadata(singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema_version INTEGER NOT NULL CHECK(schema_version=29), extractor_version TEXT NOT NULL CHECK(extractor_version='native-v6-delta-v1'), root_spelling TEXT NOT NULL, root_device TEXT NOT NULL, root_inode TEXT NOT NULL, index_generation TEXT NOT NULL, index_revision INTEGER NOT NULL CHECK(index_revision BETWEEN 0 AND 9007199254740991), last_opened_at INTEGER NOT NULL CHECK(last_opened_at BETWEEN 0 AND 9007199254740991), indexed_at TEXT NOT NULL, stats TEXT NOT NULL, diagnostics TEXT NOT NULL, reconcile_options TEXT);
CREATE TABLE native_producers(id TEXT NOT NULL,version TEXT NOT NULL,executable_hash TEXT NOT NULL CHECK(length(executable_hash)=64),kind TEXT NOT NULL CHECK(kind='native'),position_encoding TEXT NOT NULL CHECK(position_encoding='utf8'),PRIMARY KEY(id,version));
CREATE TABLE native_producer_languages(producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,language TEXT NOT NULL,inventory_authenticated INTEGER NOT NULL CHECK(inventory_authenticated IN (0,1)),ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(producer_id,producer_version,language),UNIQUE(producer_id,producer_version,ordinal),FOREIGN KEY(producer_id,producer_version) REFERENCES native_producers(id,version) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_producer_inputs(producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,language TEXT NOT NULL,component_name TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(producer_id,producer_version,language,component_name),UNIQUE(producer_id,producer_version,language,ordinal),FOREIGN KEY(producer_id,producer_version,language) REFERENCES native_producer_languages(producer_id,producer_version,language) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_source_sets(id TEXT PRIMARY KEY,root_id TEXT NOT NULL);
CREATE TABLE native_source_set_languages(source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,language TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(source_set_id,language),UNIQUE(source_set_id,ordinal));
CREATE TABLE native_source_set_dependencies(source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,dependency_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(source_set_id,dependency_id),UNIQUE(source_set_id,ordinal));
CREATE TABLE native_revision_supersessions(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id),superseded_at INTEGER NOT NULL CHECK(superseded_at BETWEEN 0 AND 9007199254740991),state TEXT NOT NULL DEFAULT 'retained' CHECK(state IN ('retained','pending','released')));
CREATE INDEX native_supersessions_clock ON native_revision_supersessions(superseded_at,revision_id);
CREATE INDEX native_supersessions_due ON native_revision_supersessions(superseded_at,revision_id) WHERE state='retained';
CREATE TABLE native_revision_release_debt(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,phase TEXT NOT NULL CHECK(phase='pending'));
CREATE TABLE native_release_candidate_versions(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,vid INTEGER NOT NULL REFERENCES document_versions(vid) DEFERRABLE INITIALLY DEFERRED,PRIMARY KEY(revision_id,vid));
CREATE TABLE native_release_candidate_graphs(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,gid INTEGER NOT NULL REFERENCES graph_projections(gid) DEFERRABLE INITIALLY DEFERRED,PRIMARY KEY(revision_id,gid));
CREATE TABLE native_release_candidate_classes(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,cid INTEGER NOT NULL REFERENCES class_projections(cid) DEFERRABLE INITIALLY DEFERRED,PRIMARY KEY(revision_id,cid));
CREATE TABLE native_revisions(id TEXT PRIMARY KEY,source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,toolchain_hash TEXT NOT NULL CHECK(length(toolchain_hash)=64),config_hash TEXT NOT NULL CHECK(length(config_hash)=64),dependency_hash TEXT NOT NULL CHECK(length(dependency_hash)=64),native_revision_id TEXT NOT NULL,source_inventory TEXT NOT NULL,dependency_observations TEXT NOT NULL,retained_toolchain TEXT NOT NULL,retained_config TEXT NOT NULL,retained_dependencies TEXT NOT NULL,reconcile_options TEXT,class_warnings TEXT NOT NULL,class_truncated INTEGER NOT NULL CHECK(class_truncated IN (0,1)),graph_stats TEXT NOT NULL CHECK(json_valid(graph_stats) AND json_type(graph_stats)='object'),graph_diagnostics TEXT NOT NULL CHECK(json_valid(graph_diagnostics) AND json_type(graph_diagnostics)='array'),published_index_revision INTEGER NOT NULL CHECK(published_index_revision BETWEEN 0 AND 9007199254740991));
CREATE UNIQUE INDEX native_revisions_pin ON native_revisions(published_index_revision);
CREATE INDEX native_revisions_header_cover ON native_revisions(source_set_id,published_index_revision,id);
CREATE TABLE revision_capture_inputs(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,input_key TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(revision_id,input_key));
CREATE TABLE document_versions(vid INTEGER PRIMARY KEY,id TEXT NOT NULL UNIQUE,language TEXT NOT NULL,path TEXT NOT NULL,content_hash TEXT NOT NULL CHECK(length(content_hash)=64),extraction_context TEXT NOT NULL CHECK(length(extraction_context)=64),producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,byte_length INTEGER NOT NULL CHECK(byte_length>=0 AND byte_length=length(source_bytes)),source_bytes BLOB NOT NULL,native_witness TEXT NOT NULL CHECK(length(native_witness)=64),UNIQUE(language,path,content_hash,extraction_context,producer_id,producer_version),UNIQUE(vid,language,path),UNIQUE(vid,language),FOREIGN KEY(producer_id,producer_version,language) REFERENCES native_producer_languages(producer_id,producer_version,language) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE graph_projections(gid INTEGER PRIMARY KEY,vid INTEGER NOT NULL,language TEXT NOT NULL,graph_hash TEXT NOT NULL UNIQUE CHECK(length(graph_hash)=64),state TEXT NOT NULL CHECK(state IN ('staged','ready','unavailable')),class_extraction_state TEXT NOT NULL CHECK(class_extraction_state IN ('notApplicable','ready')),class_extraction_payload TEXT,class_summary_payload TEXT,CHECK((class_extraction_state='ready')=(class_extraction_payload IS NOT NULL)),CHECK((class_extraction_state='ready')=(class_summary_payload IS NOT NULL)),CHECK((language IN ('java','python'))=(class_extraction_state IN ('ready'))),UNIQUE(vid,graph_hash),UNIQUE(gid,vid),FOREIGN KEY(vid,language) REFERENCES document_versions(vid,language) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE graph_nodes(gid INTEGER NOT NULL REFERENCES graph_projections(gid) DEFERRABLE INITIALLY DEFERRED,id BLOB NOT NULL,name TEXT NOT NULL,kind TEXT NOT NULL,PRIMARY KEY(gid,id));
CREATE INDEX graph_nodes_search_name ON graph_nodes(name COLLATE BINARY,id,gid);
CREATE INDEX graph_nodes_search_folded ON graph_nodes(lower(name),name COLLATE BINARY,id,gid);
CREATE TABLE graph_display_labels(gid INTEGER NOT NULL,id BLOB NOT NULL,label TEXT NOT NULL,PRIMARY KEY(gid,id),FOREIGN KEY(gid,id) REFERENCES graph_nodes(gid,id) DEFERRABLE INITIALLY DEFERRED);
CREATE VIRTUAL TABLE graph_node_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');
CREATE TRIGGER graph_node_trigrams_insert AFTER INSERT ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TRIGGER graph_node_trigrams_delete AFTER DELETE ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(graph_node_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); END;
CREATE TRIGGER graph_node_trigrams_update AFTER UPDATE OF name ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(graph_node_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); INSERT INTO graph_node_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TABLE class_projections(cid INTEGER PRIMARY KEY,gid INTEGER NOT NULL REFERENCES graph_projections(gid) DEFERRABLE INITIALLY DEFERRED,content_hash TEXT NOT NULL UNIQUE CHECK(length(content_hash)=64),state TEXT NOT NULL CHECK(state IN ('ready','unavailable')),UNIQUE(gid,content_hash),UNIQUE(cid,gid));
CREATE TABLE classes(cid INTEGER NOT NULL,gid INTEGER NOT NULL,id BLOB NOT NULL,name TEXT NOT NULL,qualified_name TEXT NOT NULL,path TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(cid,id),FOREIGN KEY(cid,gid) REFERENCES class_projections(cid,gid) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(gid,id) REFERENCES graph_nodes(gid,id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE class_relations(cid INTEGER NOT NULL,id TEXT NOT NULL,owner BLOB NOT NULL,target BLOB,payload TEXT NOT NULL,PRIMARY KEY(cid,id),FOREIGN KEY(cid,owner) REFERENCES classes(cid,id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(cid,target) REFERENCES classes(cid,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX class_relations_owner ON class_relations(cid,owner,id);
CREATE INDEX class_relations_target ON class_relations(cid,target,id);
CREATE TABLE revision_documents(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,language TEXT NOT NULL,path TEXT NOT NULL,vid INTEGER NOT NULL,gid INTEGER NOT NULL,cid INTEGER,capture_stat TEXT,coverage_requested INTEGER NOT NULL CHECK(coverage_requested IN (0,1)),coverage_selected INTEGER NOT NULL CHECK(coverage_selected IN (0,1)),coverage_state TEXT NOT NULL CHECK(coverage_state IN ('notRequested','omitted','unsupported','failed','partial','complete')),coverage_diagnostic TEXT,ordinal INTEGER NOT NULL CHECK(ordinal>=0),class_complete INTEGER NOT NULL CHECK(class_complete IN (0,1)),CHECK((coverage_state='complete')=(coverage_diagnostic IS NULL)),PRIMARY KEY(revision_id,language,path),UNIQUE(revision_id,ordinal),FOREIGN KEY(vid,language,path) REFERENCES document_versions(vid,language,path) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(gid,vid) REFERENCES graph_projections(gid,vid) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(cid,gid) REFERENCES class_projections(cid,gid) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX revision_documents_version ON revision_documents(vid,revision_id);
CREATE INDEX revision_documents_graph ON revision_documents(gid,revision_id);
CREATE INDEX revision_documents_class ON revision_documents(cid,revision_id);
CREATE INDEX revision_documents_path ON revision_documents(revision_id,path);
CREATE TABLE native_version_coverage_roles(vid INTEGER NOT NULL REFERENCES document_versions(vid) DEFERRABLE INITIALLY DEFERRED,role_kind TEXT NOT NULL CHECK(role_kind IN ('supported','observed')),role TEXT NOT NULL CHECK(role IN ('definition','call')),ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(vid,role_kind,ordinal),UNIQUE(vid,role_kind,role));
CREATE TABLE native_version_declarations(vid INTEGER NOT NULL REFERENCES document_versions(vid) DEFERRABLE INITIALLY DEFERRED,syntax_id BLOB NOT NULL,owner_syntax_id BLOB,kind TEXT NOT NULL,role TEXT NOT NULL CHECK(role IN ('test','production')),name TEXT,lookup_key TEXT,key_signature_present INTEGER NOT NULL CHECK(key_signature_present IN (0,1)),key_type_parameter_count INTEGER,key_variadic INTEGER,key_ordinal INTEGER NOT NULL CHECK(key_ordinal>=0),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,name_start INTEGER,name_end INTEGER,CHECK(start_byte>=0 AND end_byte>=start_byte),CHECK((name IS NULL)=(lookup_key IS NULL) AND (name IS NULL)=(name_start IS NULL) AND (name_start IS NULL)=(name_end IS NULL)),CHECK(name_start IS NULL OR (name_start>=start_byte AND name_end<=end_byte AND name_end>name_start)),CHECK((key_signature_present=0 AND key_type_parameter_count IS NULL AND key_variadic IS NULL) OR (key_signature_present=1 AND key_type_parameter_count>=0 AND key_variadic IN (0,1))),PRIMARY KEY(vid,syntax_id),FOREIGN KEY(vid,owner_syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_declarations_lookup ON native_version_declarations(lookup_key,vid,syntax_id);
CREATE INDEX native_version_declarations_search_name ON native_version_declarations(name COLLATE BINARY,syntax_id,vid);
CREATE INDEX native_version_declarations_search_folded ON native_version_declarations(lower(name),name COLLATE BINARY,syntax_id,vid);
CREATE INDEX native_version_declarations_owner ON native_version_declarations(vid,owner_syntax_id);
CREATE TABLE native_version_search_cards(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,card_text TEXT NOT NULL,card_hash TEXT NOT NULL CHECK(length(card_hash)=64),facts_json TEXT NOT NULL CHECK(json_valid(facts_json)),terms TEXT NOT NULL,PRIMARY KEY(vid,syntax_id),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_search_cards_cursor ON native_version_search_cards(syntax_id,vid);
CREATE VIRTUAL TABLE head_search_fts USING fts5(terms,vid UNINDEXED,syntax_id UNINDEXED,tokenize='porter unicode61 remove_diacritics 2');
CREATE VIRTUAL TABLE native_declaration_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');
CREATE TRIGGER native_declaration_trigrams_insert AFTER INSERT ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TRIGGER native_declaration_trigrams_delete AFTER DELETE ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(native_declaration_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); END;
CREATE TRIGGER native_declaration_trigrams_update AFTER UPDATE OF name ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(native_declaration_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); INSERT INTO native_declaration_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TABLE native_version_declaration_ancestors(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),kind TEXT NOT NULL,name TEXT,sibling_ordinal INTEGER NOT NULL CHECK(sibling_ordinal>=0),signature_present INTEGER NOT NULL CHECK(signature_present IN (0,1)),type_parameter_count INTEGER,variadic INTEGER,CHECK((signature_present=0 AND type_parameter_count IS NULL AND variadic IS NULL) OR (signature_present=1 AND type_parameter_count>=0 AND variadic IN (0,1))),PRIMARY KEY(vid,syntax_id,ordinal),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_own_signature_types(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),type_name TEXT NOT NULL,PRIMARY KEY(vid,syntax_id,ordinal),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_ancestor_signature_types(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,ancestor_ordinal INTEGER NOT NULL CHECK(ancestor_ordinal>=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),type_name TEXT NOT NULL,PRIMARY KEY(vid,syntax_id,ancestor_ordinal,ordinal),FOREIGN KEY(vid,syntax_id,ancestor_ordinal) REFERENCES native_version_declaration_ancestors(vid,syntax_id,ordinal) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_headers(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,kind TEXT NOT NULL,name TEXT,result_type TEXT,PRIMARY KEY(vid,syntax_id),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_header_items(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,item_kind TEXT NOT NULL CHECK(item_kind IN ('modifier','typeParameter','base')),ordinal INTEGER NOT NULL CHECK(ordinal>=0),value TEXT NOT NULL,PRIMARY KEY(vid,syntax_id,item_kind,ordinal),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_headers(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_parameters(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),name TEXT,type_name TEXT,variadic INTEGER NOT NULL CHECK(variadic IN (0,1)),PRIMARY KEY(vid,syntax_id,ordinal),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_headers(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_references(vid INTEGER NOT NULL,id BLOB NOT NULL,owner_syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,spelling TEXT NOT NULL,lookup_key TEXT,CHECK(start_byte>=0 AND end_byte>start_byte),PRIMARY KEY(vid,owner_syntax_id,ordinal),FOREIGN KEY(vid,owner_syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED) WITHOUT ROWID;
CREATE TABLE native_version_capability_families(vid INTEGER NOT NULL REFERENCES document_versions(vid) DEFERRABLE INITIALLY DEFERRED,kind TEXT NOT NULL CHECK(kind IN ('declarationName','callee','invocation','reference')),available INTEGER NOT NULL CHECK(available IN (0,1)),diagnostic TEXT,ordinal INTEGER NOT NULL CHECK(ordinal>=0),CHECK((available=1 AND diagnostic IS NULL) OR (available=0 AND length(diagnostic)>0)),PRIMARY KEY(vid,kind),UNIQUE(vid,ordinal));
CREATE TABLE native_version_capability_gaps(vid INTEGER NOT NULL,kind TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,diagnostic TEXT NOT NULL CHECK(length(diagnostic)>0),CHECK(start_byte>=0 AND end_byte>start_byte),PRIMARY KEY(vid,kind,ordinal),FOREIGN KEY(vid,kind) REFERENCES native_version_capability_families(vid,kind) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_calls(vid INTEGER NOT NULL,id BLOB NOT NULL,owner_syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,callee_start INTEGER,callee_end INTEGER,spelling TEXT,language TEXT NOT NULL,callee_lookup_key TEXT,CHECK(start_byte>=0 AND end_byte>start_byte),CHECK((callee_start IS NULL)=(callee_end IS NULL)),CHECK(callee_start IS NULL OR (callee_start>=start_byte AND callee_end<=end_byte AND callee_end>callee_start)),PRIMARY KEY(vid,id),UNIQUE(vid,owner_syntax_id,ordinal),FOREIGN KEY(vid,owner_syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_calls_incoming ON native_version_calls(language,callee_lookup_key);
CREATE TABLE native_version_control_regions(vid INTEGER NOT NULL,id BLOB NOT NULL,owner_syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),kind TEXT NOT NULL,start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,parent_id BLOB,arm TEXT,CHECK(start_byte>=0 AND end_byte>start_byte),PRIMARY KEY(vid,id),UNIQUE(vid,owner_syntax_id,ordinal),UNIQUE(vid,id,owner_syntax_id),FOREIGN KEY(vid,owner_syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(vid,parent_id,owner_syntax_id) REFERENCES native_version_control_regions(vid,id,owner_syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_regions_parent ON native_version_control_regions(vid,parent_id,owner_syntax_id);
CREATE TABLE native_version_call_regions(vid INTEGER NOT NULL,call_id BLOB NOT NULL,region_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(vid,call_id,ordinal),UNIQUE(vid,call_id,region_id),FOREIGN KEY(vid,call_id) REFERENCES native_version_calls(vid,id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(vid,region_id) REFERENCES native_version_control_regions(vid,id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_binding_epoch(singleton INTEGER PRIMARY KEY CHECK(singleton=1),index_generation TEXT NOT NULL,first_revision INTEGER NOT NULL CHECK(first_revision BETWEEN 1 AND 9007199254740991));
CREATE TABLE revision_producer_bindings(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,producer_sha TEXT NOT NULL CHECK(length(producer_sha)=64),binding_sha TEXT NOT NULL CHECK(length(binding_sha)=64));
CREATE TABLE semantic_producers(id TEXT NOT NULL,version TEXT NOT NULL,executable_hash TEXT NOT NULL CHECK(length(executable_hash)=64),kind TEXT NOT NULL CHECK(kind='semantic'),position_encoding TEXT NOT NULL CHECK(position_encoding IN ('utf8','utf16','unicodeScalar')),identity_kind TEXT NOT NULL CHECK(identity_kind IN ('executable','generatorSource')),PRIMARY KEY(id,version));
CREATE TABLE semantic_producer_languages(id TEXT NOT NULL,version TEXT NOT NULL,language TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 9007199254740991),PRIMARY KEY(id,version,language),UNIQUE(id,version,ordinal),FOREIGN KEY(id,version) REFERENCES semantic_producers(id,version));
CREATE TABLE semantic_revision_producers(revision_id TEXT NOT NULL REFERENCES native_revisions(id),id TEXT NOT NULL,version TEXT NOT NULL,PRIMARY KEY(revision_id,id),UNIQUE(revision_id,id,version),FOREIGN KEY(id,version) REFERENCES semantic_producers(id,version));
CREATE TABLE semantic_requested_producers(revision_id TEXT NOT NULL REFERENCES native_revisions(id),id TEXT NOT NULL,version TEXT NOT NULL,executable_hash TEXT NOT NULL CHECK(length(executable_hash)=64),PRIMARY KEY(revision_id,id));
CREATE TABLE semantic_requested_producer_languages(revision_id TEXT NOT NULL,id TEXT NOT NULL,language TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 9007199254740991),PRIMARY KEY(revision_id,id,language),UNIQUE(revision_id,id,ordinal),FOREIGN KEY(revision_id,id) REFERENCES semantic_requested_producers(revision_id,id));
CREATE TABLE semantic_coverage(tuple_id INTEGER PRIMARY KEY,revision_id TEXT NOT NULL,producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,language TEXT NOT NULL,path TEXT NOT NULL,requested INTEGER NOT NULL CHECK(requested IN (0,1)),selected INTEGER NOT NULL CHECK(selected IN (0,1)),state TEXT NOT NULL CHECK(state IN ('notRequested','omitted','unsupported','failed','partial','complete')),diagnostic TEXT,UNIQUE(revision_id,producer_id,language,path),UNIQUE(tuple_id,revision_id),FOREIGN KEY(revision_id) REFERENCES native_revisions(id),FOREIGN KEY(producer_id,producer_version,language) REFERENCES semantic_producer_languages(id,version,language),FOREIGN KEY(revision_id,producer_id,producer_version) REFERENCES semantic_revision_producers(revision_id,id,version),FOREIGN KEY(revision_id,language,path) REFERENCES revision_documents(revision_id,language,path));
CREATE INDEX semantic_coverage_history ON semantic_coverage(producer_id,language,path,revision_id);
CREATE TABLE semantic_coverage_roles(tuple_id INTEGER NOT NULL REFERENCES semantic_coverage(tuple_id),kind TEXT NOT NULL CHECK(kind IN ('supported','observed')),ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 9007199254740991),role TEXT NOT NULL CHECK(role IN ('definition','read','write','call','type','import','alias')),PRIMARY KEY(tuple_id,kind,ordinal),UNIQUE(tuple_id,kind,role));
CREATE TABLE semantic_basis(basis_id INTEGER PRIMARY KEY,tuple_id INTEGER NOT NULL REFERENCES semantic_coverage(tuple_id),producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,producer_hash TEXT NOT NULL,artifact_hash TEXT NOT NULL,language TEXT NOT NULL,source_set_id TEXT NOT NULL,captured_native_revision_id TEXT NOT NULL,source_manifest_hash TEXT NOT NULL,toolchain_hash TEXT NOT NULL,config_hash TEXT NOT NULL,dependency_hash TEXT NOT NULL,UNIQUE(tuple_id,basis_id));
CREATE TABLE semantic_basis_lookup_dependencies(basis_id INTEGER NOT NULL REFERENCES semantic_basis(basis_id),ordinal INTEGER NOT NULL CHECK(ordinal BETWEEN 0 AND 9007199254740991),lookup_key TEXT NOT NULL,PRIMARY KEY(basis_id,ordinal),UNIQUE(basis_id,lookup_key));
CREATE TABLE semantic_raw_batches(batch_id INTEGER PRIMARY KEY,producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,profile_id TEXT NOT NULL,source_set_id TEXT NOT NULL,captured_revision TEXT NOT NULL,source_manifest_hash TEXT NOT NULL CHECK(length(source_manifest_hash)=64),descriptor_sha256 TEXT NOT NULL CHECK(length(descriptor_sha256)=64),descriptor_bytes BLOB NOT NULL,decoder_version TEXT NOT NULL,UNIQUE(producer_id,descriptor_sha256,captured_revision));
CREATE TABLE semantic_raw_captures(capture_id INTEGER PRIMARY KEY,batch_id INTEGER NOT NULL REFERENCES semantic_raw_batches(batch_id),ordinal INTEGER NOT NULL,language TEXT NOT NULL,producer_id TEXT NOT NULL,artifact_hash TEXT NOT NULL CHECK(length(artifact_hash)=64),source_set_id TEXT NOT NULL,captured_revision TEXT NOT NULL,binary_bytes BLOB NOT NULL,decoder_version TEXT NOT NULL,UNIQUE(batch_id,ordinal),UNIQUE(producer_id,artifact_hash,source_set_id,captured_revision));
CREATE INDEX semantic_raw_captures_batch ON semantic_raw_captures(batch_id);
CREATE TABLE semantic_raw_exclusions(batch_id INTEGER NOT NULL REFERENCES semantic_raw_batches(batch_id),language TEXT NOT NULL,path TEXT NOT NULL,reason TEXT NOT NULL CHECK(reason IN ('notRequested','unsupported','failed')),PRIMARY KEY(batch_id,language,path));
CREATE TABLE semantic_revision_raw_selections(revision_id TEXT NOT NULL REFERENCES native_revisions(id),producer_id TEXT NOT NULL,batch_id INTEGER NOT NULL REFERENCES semantic_raw_batches(batch_id),captured_pin TEXT NOT NULL REFERENCES native_revisions(id),PRIMARY KEY(revision_id,producer_id));
CREATE INDEX semantic_raw_selections_batch ON semantic_revision_raw_selections(batch_id);
CREATE INDEX semantic_raw_selections_captured_pin ON semantic_revision_raw_selections(captured_pin);
CREATE TABLE semantic_raw_proofs(capture_id INTEGER NOT NULL REFERENCES semantic_raw_captures(capture_id),proof_id TEXT NOT NULL,record_kind TEXT NOT NULL CHECK(record_kind IN ('occurrence','symbolInformation','relationship')),fact_digest TEXT NOT NULL CHECK(length(fact_digest)=64),language TEXT NOT NULL,path TEXT NOT NULL,canonical_bytes BLOB NOT NULL,UNIQUE(capture_id,proof_id),UNIQUE(capture_id,language,path,record_kind,fact_digest));
CREATE TABLE semantic_raw_documents(capture_id INTEGER NOT NULL REFERENCES semantic_raw_captures(capture_id) ON DELETE CASCADE,language TEXT NOT NULL,path TEXT NOT NULL,source_hash TEXT NOT NULL CHECK(length(source_hash)=64),wire_digest TEXT NOT NULL CHECK(length(wire_digest)=64),capped INTEGER NOT NULL CHECK(capped IN (0,1)),PRIMARY KEY(capture_id,language,path));
CREATE TABLE semantic_import_reports(revision_id TEXT NOT NULL REFERENCES native_revisions(id),producer_id TEXT NOT NULL,report_json TEXT NOT NULL CHECK(json_valid(report_json)),PRIMARY KEY(revision_id,producer_id));
CREATE TABLE semantic_provenance(tuple_id INTEGER NOT NULL,provenance_id TEXT NOT NULL,content_hash TEXT NOT NULL,evidence_kind TEXT NOT NULL CHECK(evidence_kind IN ('measuredSyntax','declarationBinding','semanticReference','typeRelationship')),freshness TEXT NOT NULL CHECK(freshness IN ('fresh','possiblyStale','stale')),basis_id INTEGER NOT NULL,capture_id INTEGER,derived_from_provenance_id TEXT,derived_from_record_kind TEXT CHECK(derived_from_record_kind IN ('occurrence','symbolInformation','relationship')),derived_from_fact_digest TEXT CHECK(length(derived_from_fact_digest)=64),CHECK((capture_id IS NULL AND derived_from_provenance_id IS NULL AND derived_from_record_kind IS NULL AND derived_from_fact_digest IS NULL) OR (capture_id IS NOT NULL AND derived_from_provenance_id IS NOT NULL AND derived_from_record_kind IS NOT NULL AND derived_from_fact_digest IS NOT NULL)),PRIMARY KEY(tuple_id,provenance_id),FOREIGN KEY(tuple_id) REFERENCES semantic_coverage(tuple_id),FOREIGN KEY(tuple_id,basis_id) REFERENCES semantic_basis(tuple_id,basis_id),FOREIGN KEY(capture_id,derived_from_provenance_id) REFERENCES semantic_raw_proofs(capture_id,proof_id));
CREATE TABLE semantic_symbols(symbol_id INTEGER PRIMARY KEY,tuple_id INTEGER NOT NULL,ordinal INTEGER NOT NULL,key_scheme TEXT NOT NULL,key_symbol TEXT NOT NULL,key_scope TEXT NOT NULL CHECK(key_scope IN ('global','document','target')),key_document_language TEXT,key_document_path TEXT,display_name TEXT,provenance_id TEXT NOT NULL,UNIQUE(tuple_id,ordinal),FOREIGN KEY(tuple_id,provenance_id) REFERENCES semantic_provenance(tuple_id,provenance_id));
CREATE UNIQUE INDEX semantic_symbols_key ON semantic_symbols(tuple_id,key_scheme,key_symbol,key_scope,COALESCE(key_document_language,''),COALESCE(key_document_path,''));
CREATE TABLE semantic_symbol_targets(symbol_id INTEGER NOT NULL REFERENCES semantic_symbols(symbol_id),ordinal INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind IN ('internal','external')),internal_syntax_id BLOB,internal_language TEXT,internal_path TEXT,internal_native_revision_id TEXT,external_scheme TEXT,external_symbol TEXT,external_scope TEXT,external_language TEXT,external_path TEXT,CHECK((kind='internal' AND length(internal_syntax_id)=16 AND internal_language IS NOT NULL AND internal_path IS NOT NULL AND internal_native_revision_id IS NOT NULL AND external_scheme IS NULL AND external_symbol IS NULL AND external_scope IS NULL AND external_language IS NULL AND external_path IS NULL) OR (kind='external' AND internal_syntax_id IS NULL AND internal_language IS NULL AND internal_path IS NULL AND internal_native_revision_id IS NULL AND external_scheme IS NOT NULL AND external_symbol IS NOT NULL AND external_scope IN ('global','document') AND ((external_scope='global' AND external_language IS NULL AND external_path IS NULL) OR (external_scope='document' AND external_language IS NOT NULL AND external_path IS NOT NULL)))),PRIMARY KEY(symbol_id,ordinal));
CREATE INDEX semantic_symbol_targets_internal_syntax ON semantic_symbol_targets(internal_syntax_id,internal_language,internal_path,symbol_id) WHERE kind='internal';
CREATE TABLE semantic_declaration_bindings(binding_id INTEGER PRIMARY KEY,tuple_id INTEGER NOT NULL,ordinal INTEGER NOT NULL,syntax_id BLOB CHECK(syntax_id IS NULL OR length(syntax_id)=16),provenance_id TEXT NOT NULL,join_document_language TEXT NOT NULL,join_document_path TEXT NOT NULL,join_native_revision_id TEXT NOT NULL,join_content_hash TEXT NOT NULL,join_start INTEGER NOT NULL,join_end INTEGER NOT NULL,join_kind TEXT NOT NULL CHECK(join_kind='declarationName'),join_status TEXT NOT NULL CHECK(join_status IN ('exact','ambiguous','unmatched','unsupported')),join_diagnostic TEXT,UNIQUE(tuple_id,ordinal),FOREIGN KEY(tuple_id,provenance_id) REFERENCES semantic_provenance(tuple_id,provenance_id));
CREATE INDEX semantic_declaration_bindings_syntax ON semantic_declaration_bindings(tuple_id,syntax_id);
CREATE TABLE semantic_declaration_binding_symbols(binding_id INTEGER NOT NULL REFERENCES semantic_declaration_bindings(binding_id),ordinal INTEGER NOT NULL,scheme TEXT NOT NULL,symbol TEXT NOT NULL,scope TEXT NOT NULL,document_language TEXT,document_path TEXT,PRIMARY KEY(binding_id,ordinal));
CREATE TABLE semantic_declaration_join_candidates(binding_id INTEGER NOT NULL REFERENCES semantic_declaration_bindings(binding_id),ordinal INTEGER NOT NULL,candidate_id BLOB NOT NULL CHECK(length(candidate_id)=16),PRIMARY KEY(binding_id,ordinal),UNIQUE(binding_id,candidate_id));
CREATE TABLE semantic_references(tuple_id INTEGER NOT NULL,reference_id BLOB NOT NULL CHECK(length(reference_id)=16),ordinal INTEGER NOT NULL,owner_id BLOB NOT NULL CHECK(length(owner_id)=16),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,spelling TEXT NOT NULL,lookup_key TEXT NOT NULL,site TEXT NOT NULL CHECK(site IN ('declaration','use')),resolution TEXT NOT NULL CHECK(resolution IN ('resolved','external','ambiguous','unresolved')),declared_kind TEXT CHECK(declared_kind IS NULL OR declared_kind IN ('internal','external')),declared_internal_syntax_id BLOB,declared_internal_language TEXT,declared_internal_path TEXT,declared_internal_native_revision_id TEXT,declared_external_scheme TEXT,declared_external_symbol TEXT,declared_external_scope TEXT,declared_external_language TEXT,declared_external_path TEXT,provenance_id TEXT NOT NULL,CHECK((declared_kind IS NULL AND declared_internal_syntax_id IS NULL AND declared_internal_language IS NULL AND declared_internal_path IS NULL AND declared_internal_native_revision_id IS NULL AND declared_external_scheme IS NULL AND declared_external_symbol IS NULL AND declared_external_scope IS NULL AND declared_external_language IS NULL AND declared_external_path IS NULL) OR (declared_kind IS NOT NULL AND declared_kind='internal' AND length(declared_internal_syntax_id)=16 AND declared_internal_language IS NOT NULL AND declared_internal_path IS NOT NULL AND declared_internal_native_revision_id IS NOT NULL AND declared_external_scheme IS NULL AND declared_external_symbol IS NULL AND declared_external_scope IS NULL AND declared_external_language IS NULL AND declared_external_path IS NULL) OR (declared_kind IS NOT NULL AND declared_kind='external' AND declared_internal_syntax_id IS NULL AND declared_internal_language IS NULL AND declared_internal_path IS NULL AND declared_internal_native_revision_id IS NULL AND declared_external_scheme IS NOT NULL AND declared_external_symbol IS NOT NULL AND declared_external_scope IN ('global','document') AND ((declared_external_scope='global' AND declared_external_language IS NULL AND declared_external_path IS NULL) OR (declared_external_scope='document' AND declared_external_language IS NOT NULL AND declared_external_path IS NOT NULL)))),PRIMARY KEY(tuple_id,reference_id),FOREIGN KEY(tuple_id,provenance_id) REFERENCES semantic_provenance(tuple_id,provenance_id));
CREATE INDEX semantic_references_owner ON semantic_references(tuple_id,owner_id);
CREATE TABLE semantic_reference_roles(tuple_id INTEGER NOT NULL,reference_id BLOB NOT NULL,ordinal INTEGER NOT NULL,role TEXT NOT NULL,PRIMARY KEY(tuple_id,reference_id,ordinal),UNIQUE(tuple_id,reference_id,role),FOREIGN KEY(tuple_id,reference_id) REFERENCES semantic_references(tuple_id,reference_id));
CREATE TABLE semantic_reference_candidates(tuple_id INTEGER NOT NULL,reference_id BLOB NOT NULL,ordinal INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind IN ('internal','external')),internal_syntax_id BLOB,internal_language TEXT,internal_path TEXT,internal_native_revision_id TEXT,external_scheme TEXT,external_symbol TEXT,external_scope TEXT,external_language TEXT,external_path TEXT,CHECK((kind='internal' AND length(internal_syntax_id)=16 AND internal_language IS NOT NULL AND internal_path IS NOT NULL AND internal_native_revision_id IS NOT NULL AND external_scheme IS NULL AND external_symbol IS NULL AND external_scope IS NULL AND external_language IS NULL AND external_path IS NULL) OR (kind='external' AND internal_syntax_id IS NULL AND internal_language IS NULL AND internal_path IS NULL AND internal_native_revision_id IS NULL AND external_scheme IS NOT NULL AND external_symbol IS NOT NULL AND external_scope IN ('global','document') AND ((external_scope='global' AND external_language IS NULL AND external_path IS NULL) OR (external_scope='document' AND external_language IS NOT NULL AND external_path IS NOT NULL)))),PRIMARY KEY(tuple_id,reference_id,ordinal),FOREIGN KEY(tuple_id,reference_id) REFERENCES semantic_references(tuple_id,reference_id));
CREATE TABLE semantic_call_bindings(binding_id INTEGER PRIMARY KEY,tuple_id INTEGER NOT NULL,ordinal INTEGER NOT NULL,call_id BLOB CHECK(call_id IS NULL OR length(call_id)=16),join_document_language TEXT NOT NULL,join_document_path TEXT NOT NULL,join_native_revision_id TEXT NOT NULL,join_content_hash TEXT NOT NULL,join_start INTEGER NOT NULL,join_end INTEGER NOT NULL,join_kind TEXT NOT NULL CHECK(join_kind IN ('callee','invocation')),join_status TEXT NOT NULL,join_diagnostic TEXT,resolution TEXT NOT NULL,declared_kind TEXT CHECK(declared_kind IS NULL OR declared_kind IN ('internal','external')),declared_internal_syntax_id BLOB,declared_internal_language TEXT,declared_internal_path TEXT,declared_internal_native_revision_id TEXT,declared_external_scheme TEXT,declared_external_symbol TEXT,declared_external_scope TEXT,declared_external_language TEXT,declared_external_path TEXT,dispatch TEXT NOT NULL,possible_dispatch_complete INTEGER NOT NULL CHECK(possible_dispatch_complete=0),stale_target INTEGER CHECK(stale_target IS NULL OR stale_target=0),provenance_id TEXT NOT NULL,CHECK((declared_kind IS NULL AND declared_internal_syntax_id IS NULL AND declared_internal_language IS NULL AND declared_internal_path IS NULL AND declared_internal_native_revision_id IS NULL AND declared_external_scheme IS NULL AND declared_external_symbol IS NULL AND declared_external_scope IS NULL AND declared_external_language IS NULL AND declared_external_path IS NULL) OR (declared_kind IS NOT NULL AND declared_kind='internal' AND length(declared_internal_syntax_id)=16 AND declared_internal_language IS NOT NULL AND declared_internal_path IS NOT NULL AND declared_internal_native_revision_id IS NOT NULL AND declared_external_scheme IS NULL AND declared_external_symbol IS NULL AND declared_external_scope IS NULL AND declared_external_language IS NULL AND declared_external_path IS NULL) OR (declared_kind IS NOT NULL AND declared_kind='external' AND declared_internal_syntax_id IS NULL AND declared_internal_language IS NULL AND declared_internal_path IS NULL AND declared_internal_native_revision_id IS NULL AND declared_external_scheme IS NOT NULL AND declared_external_symbol IS NOT NULL AND declared_external_scope IN ('global','document') AND ((declared_external_scope='global' AND declared_external_language IS NULL AND declared_external_path IS NULL) OR (declared_external_scope='document' AND declared_external_language IS NOT NULL AND declared_external_path IS NOT NULL)))),UNIQUE(tuple_id,ordinal),FOREIGN KEY(tuple_id,provenance_id) REFERENCES semantic_provenance(tuple_id,provenance_id));
CREATE INDEX semantic_call_bindings_call ON semantic_call_bindings(tuple_id,call_id);
CREATE TABLE semantic_call_candidates(binding_id INTEGER NOT NULL REFERENCES semantic_call_bindings(binding_id),ordinal INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind IN ('internal','external')),internal_syntax_id BLOB,internal_language TEXT,internal_path TEXT,internal_native_revision_id TEXT,external_scheme TEXT,external_symbol TEXT,external_scope TEXT,external_language TEXT,external_path TEXT,CHECK((kind='internal' AND length(internal_syntax_id)=16 AND internal_language IS NOT NULL AND internal_path IS NOT NULL AND internal_native_revision_id IS NOT NULL AND external_scheme IS NULL AND external_symbol IS NULL AND external_scope IS NULL AND external_language IS NULL AND external_path IS NULL) OR (kind='external' AND internal_syntax_id IS NULL AND internal_language IS NULL AND internal_path IS NULL AND internal_native_revision_id IS NULL AND external_scheme IS NOT NULL AND external_symbol IS NOT NULL AND external_scope IN ('global','document') AND ((external_scope='global' AND external_language IS NULL AND external_path IS NULL) OR (external_scope='document' AND external_language IS NOT NULL AND external_path IS NOT NULL)))),PRIMARY KEY(binding_id,ordinal));
CREATE TABLE semantic_call_possible_dispatch(binding_id INTEGER NOT NULL REFERENCES semantic_call_bindings(binding_id),ordinal INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind IN ('internal','external')),internal_syntax_id BLOB,internal_language TEXT,internal_path TEXT,internal_native_revision_id TEXT,external_scheme TEXT,external_symbol TEXT,external_scope TEXT,external_language TEXT,external_path TEXT,CHECK((kind='internal' AND length(internal_syntax_id)=16 AND internal_language IS NOT NULL AND internal_path IS NOT NULL AND internal_native_revision_id IS NOT NULL AND external_scheme IS NULL AND external_symbol IS NULL AND external_scope IS NULL AND external_language IS NULL AND external_path IS NULL) OR (kind='external' AND internal_syntax_id IS NULL AND internal_language IS NULL AND internal_path IS NULL AND internal_native_revision_id IS NULL AND external_scheme IS NOT NULL AND external_symbol IS NOT NULL AND external_scope IN ('global','document') AND ((external_scope='global' AND external_language IS NULL AND external_path IS NULL) OR (external_scope='document' AND external_language IS NOT NULL AND external_path IS NOT NULL)))),PRIMARY KEY(binding_id,ordinal));
CREATE TABLE semantic_call_join_candidates(binding_id INTEGER NOT NULL REFERENCES semantic_call_bindings(binding_id),ordinal INTEGER NOT NULL,candidate_id BLOB NOT NULL CHECK(length(candidate_id)=16),PRIMARY KEY(binding_id,ordinal),UNIQUE(binding_id,candidate_id));
CREATE TABLE semantic_type_relationships(relationship_id INTEGER PRIMARY KEY,tuple_id INTEGER NOT NULL,ordinal INTEGER NOT NULL,kind TEXT NOT NULL CHECK(kind IN ('extends','implements','overrides')),source_kind TEXT NOT NULL CHECK(source_kind IN ('internal','external')),source_internal_syntax_id BLOB,source_internal_language TEXT,source_internal_path TEXT,source_internal_native_revision_id TEXT,source_external_scheme TEXT,source_external_symbol TEXT,source_external_scope TEXT,source_external_language TEXT,source_external_path TEXT,target_kind TEXT NOT NULL CHECK(target_kind IN ('internal','external')),target_internal_syntax_id BLOB,target_internal_language TEXT,target_internal_path TEXT,target_internal_native_revision_id TEXT,target_external_scheme TEXT,target_external_symbol TEXT,target_external_scope TEXT,target_external_language TEXT,target_external_path TEXT,provenance_id TEXT NOT NULL,CHECK((source_kind='internal' AND length(source_internal_syntax_id)=16 AND source_internal_language IS NOT NULL AND source_internal_path IS NOT NULL AND source_internal_native_revision_id IS NOT NULL AND source_external_scheme IS NULL AND source_external_symbol IS NULL AND source_external_scope IS NULL AND source_external_language IS NULL AND source_external_path IS NULL) OR (source_kind='external' AND source_internal_syntax_id IS NULL AND source_internal_language IS NULL AND source_internal_path IS NULL AND source_internal_native_revision_id IS NULL AND source_external_scheme IS NOT NULL AND source_external_symbol IS NOT NULL AND source_external_scope IN ('global','document') AND ((source_external_scope='global' AND source_external_language IS NULL AND source_external_path IS NULL) OR (source_external_scope='document' AND source_external_language IS NOT NULL AND source_external_path IS NOT NULL)))),CHECK((target_kind='internal' AND length(target_internal_syntax_id)=16 AND target_internal_language IS NOT NULL AND target_internal_path IS NOT NULL AND target_internal_native_revision_id IS NOT NULL AND target_external_scheme IS NULL AND target_external_symbol IS NULL AND target_external_scope IS NULL AND target_external_language IS NULL AND target_external_path IS NULL) OR (target_kind='external' AND target_internal_syntax_id IS NULL AND target_internal_language IS NULL AND target_internal_path IS NULL AND target_internal_native_revision_id IS NULL AND target_external_scheme IS NOT NULL AND target_external_symbol IS NOT NULL AND target_external_scope IN ('global','document') AND ((target_external_scope='global' AND target_external_language IS NULL AND target_external_path IS NULL) OR (target_external_scope='document' AND target_external_language IS NOT NULL AND target_external_path IS NOT NULL)))),UNIQUE(tuple_id,ordinal),FOREIGN KEY(tuple_id,provenance_id) REFERENCES semantic_provenance(tuple_id,provenance_id));
CREATE INDEX semantic_type_relationships_source ON semantic_type_relationships(tuple_id,source_internal_syntax_id);
"#;
/// A normal connection keeps the verified index use lock until SQLite closes.
struct IndexConnection {
    db: ProtectedSqliteConnection,
    _use_guard: topology::UseGuard,
}
impl Deref for IndexConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.db
    }
}
impl DerefMut for IndexConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        &mut self.db
    }
}
// Only the live variant owns a new shared-use guard. The stage variant keeps
// the caller's verified exclusive leader guard alive across its transaction.
/// Page cache for a publication's connection, in KiB (256 MiB).
const PUBLICATION_CACHE_KIB: i64 = 256 * 1024;

enum PublicationConnection {
    Live(IndexConnection),
    Stage(ProtectedSqliteConnection),
}
impl Deref for PublicationConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        match self {
            Self::Live(db) => &db.db,
            Self::Stage(db) => db,
        }
    }
}
impl DerefMut for PublicationConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        match self {
            Self::Live(db) => &mut db.db,
            Self::Stage(db) => db,
        }
    }
}

/// How often this thread computed each per-snapshot or per-file selected value.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct SelectedReadCounts {
    producer_hashes: usize,
    source_reads: usize,
    document_reads: usize,
}
#[cfg(test)]
thread_local! {
    static SELECTED_READ_COUNTS: std::cell::Cell<SelectedReadCounts> =
        std::cell::Cell::new(SelectedReadCounts::default());
}
#[cfg(test)]
fn count_selected_read(update: impl FnOnce(&mut SelectedReadCounts)) {
    SELECTED_READ_COUNTS.with(|counts| {
        let mut value = counts.get();
        update(&mut value);
        counts.set(value);
    });
}
#[cfg(test)]
fn take_selected_read_counts() -> SelectedReadCounts {
    SELECTED_READ_COUNTS.with(|counts| counts.take())
}

/// One selected document's manifest row, after its SQL row and byte budgets
/// passed and before the rows they bound are read.
struct SelectedHeader {
    path: String,
    language: String,
    hash: String,
    bytes: i64,
    scope: V8NativeScope,
    /// The source, native and class ancillary rows, graph rows and F this
    /// document's rows hold, in stored bytes, as its budgets measured them.
    stored_bytes: u64,
}

/// One selected document's stored source and native rows,
/// read from one snapshot. The indexer validated them before publishing, so
/// reads trust them (owner ruling 2026-10-23, #261).
struct FetchedDocument {
    file: SourceFile,
    declarations: Vec<crate::native_evidence::Declaration>,
    calls: Vec<crate::native_evidence::Call>,
    regions: Vec<crate::native_evidence::ControlRegion>,
    stored_bytes: u64,
}

/// Match the former graph-row SQL charge: key TEXT byte lengths plus the
/// compact JSON payload, with identical row and aggregate thresholds.
fn derived_graph_bytes(
    nodes: &[Symbol],
    calls: &[CallSite],
    regions: &[ControlRegion],
    source_bytes: i64,
) -> Result<u64> {
    let mut max_row = 0u64;
    let mut total = 0u64;
    for node in nodes {
        let row = node.id.len() + node.name.len() + node.path.len() + json(node)?.len();
        let row = row as u64;
        max_row = max_row.max(row);
        total = total.saturating_add(row);
    }
    for call in calls {
        let row = call.id.len() + call.caller.len() + call.path.len() + json(call)?.len();
        let row = row as u64;
        max_row = max_row.max(row);
        total = total.saturating_add(row);
    }
    for region in regions {
        let row = region.id.len() + region.owner.len() + region.path.len() + json(region)?.len();
        let row = row as u64;
        max_row = max_row.max(row);
        total = total.saturating_add(row);
    }
    let bytes = source_bytes.max(0) as u64;
    ensure!(
        max_row
            <= bytes
                .saturating_mul(6)
                .saturating_add(16 * 1024)
                .min(256 * 1024 * 1024 + 16 * 1024)
            && total
                <= bytes
                    .saturating_mul(64)
                    .saturating_add(256 * 1024)
                    .min(512 * 1024 * 1024),
        "incompatible_index: selected graph row byte budget exceeded"
    );
    Ok(total)
}

/// Header of a retained native revision: everything but the document inventory.
pub struct NativeHeader {
    pub source_set: crate::native_evidence::SourceSet,
    pub revision: crate::native_evidence::Revision,
    pub producer: crate::native_evidence::Producer,
}

/// One retained native document: its exact source bytes and measured native rows.
pub struct NativeDocumentRows {
    pub source: Vec<u8>,
    pub coverage: crate::native_evidence::Coverage,
    pub provenance: crate::native_evidence::Provenance,
    pub declarations: Vec<crate::native_evidence::Declaration>,
    pub calls: Vec<crate::native_evidence::Call>,
    pub control_regions: Vec<crate::native_evidence::ControlRegion>,
    pub references: Vec<crate::native_evidence::Reference>,
    pub capability_report: crate::native_evidence::CapabilityReport,
    pub search_cards: Vec<crate::search_capture::CapturedCard>,
}

/// Reads one retained native revision inside a single read transaction, one document at a time.
pub struct NativeReader<'a> {
    store: &'a Store,
    db: &'a Connection,
    selected: &'a ReadRevision,
}

impl NativeReader<'_> {
    #[cfg(feature = "test-support")]
    pub fn cache_size_kib(&self) -> Result<i64> {
        let pages: i64 = self
            .db
            .pragma_query_value(None, "cache_size", |r| r.get(0))?;
        Ok(-pages)
    }

    pub fn header(&self) -> Result<NativeHeader> {
        Store::read_native_header(self.db, self.selected)
    }

    /// The retained source bytes of one inventory entry, checked against its identity.
    pub fn source_bytes(&self, document: &crate::native_evidence::Document) -> Result<Vec<u8>> {
        self.store
            .read_native_source(self.db, self.selected, document)
    }

    pub fn document(
        &self,
        document: &crate::native_evidence::Document,
    ) -> Result<NativeDocumentRows> {
        self.store
            .read_native_document(self.db, self.selected, document)
    }

    /// Native rows for staged joins, retaining the general reader's integrity checks. A join
    /// never reads control regions or search cards, so they are not loaded (left empty).
    pub(crate) fn join_document(
        &self,
        document: &crate::native_evidence::Document,
    ) -> Result<NativeDocumentRows> {
        self.store
            .read_native_document_rows(self.db, self.selected, document, false)
    }

    /// Logical peak estimate for native document loading, before owning its row bodies.
    pub(crate) fn document_reservation_bytes(
        &self,
        document: &crate::native_evidence::Document,
    ) -> Result<u64> {
        native_join_budget::document_bytes(self.db, self.selected, document)
    }

    /// Streams the stored source inventory through SQLite incremental blob I/O and a 1 MiB
    /// buffer, yielding one entry at a time. The array is never held whole.
    pub fn inventory(&self) -> Result<InventoryStream<'_>> {
        let rowid: i64 = self.db.query_row(
            "SELECT rowid FROM native_revisions WHERE id=?1",
            [&self.selected.key],
            |row| row.get(0),
        )?;
        let blob = self.db.blob_open(
            rusqlite::MAIN_DB,
            "native_revisions",
            "source_inventory",
            rowid,
            true,
        )?;
        Ok(InventoryStream {
            reader: std::io::BufReader::with_capacity(1 << 20, blob),
            state: InventoryState::Start,
        })
    }
}

enum InventoryState {
    Start,
    Elements,
    Done,
}

/// Pull parser over the stored `[Document, ...]` array.
pub struct InventoryStream<'a> {
    reader: std::io::BufReader<rusqlite::blob::Blob<'a>>,
    state: InventoryState,
}

impl InventoryStream<'_> {
    fn corrupt<T>() -> Result<T> {
        Err(SelectedIntegrity("incompatible_index: corrupt retained native manifest".into()).into())
    }

    /// Next non-whitespace byte, consumed.
    fn token(&mut self) -> Result<Option<u8>> {
        use std::io::BufRead;
        loop {
            let buffer = self.reader.fill_buf()?;
            let Some(&byte) = buffer.first() else {
                return Ok(None);
            };
            self.reader.consume(1);
            if !byte.is_ascii_whitespace() {
                return Ok(Some(byte));
            }
        }
    }

    pub fn next_document(&mut self) -> Result<Option<crate::native_evidence::Document>> {
        loop {
            match self.state {
                InventoryState::Done => return Ok(None),
                InventoryState::Start => {
                    if self.token()? != Some(b'[') {
                        return Self::corrupt();
                    }
                    match self.peek()? {
                        Some(b']') => {
                            self.token()?;
                            self.state = InventoryState::Done;
                        }
                        _ => self.state = InventoryState::Elements,
                    }
                }
                InventoryState::Elements => {
                    let mut elements = serde_json::Deserializer::from_reader(&mut self.reader);
                    let document = match serde::Deserialize::deserialize(&mut elements) {
                        Ok(document) => document,
                        Err(_) => return Self::corrupt(),
                    };
                    match self.token()? {
                        Some(b',') => {}
                        Some(b']') => self.state = InventoryState::Done,
                        _ => return Self::corrupt(),
                    }
                    return Ok(Some(document));
                }
            }
        }
    }

    fn peek(&mut self) -> Result<Option<u8>> {
        use std::io::BufRead;
        loop {
            let buffer = self.reader.fill_buf()?;
            let Some(&byte) = buffer.first() else {
                return Ok(None);
            };
            if !byte.is_ascii_whitespace() {
                return Ok(Some(byte));
            }
            self.reader.consume(1);
        }
    }
}

/// One selected document's source and native rows with the graph projected
/// from them once.
struct SelectedDocument {
    file: SourceFile,
    nodes: Vec<Symbol>,
    graph_calls: Vec<CallSite>,
    graph_regions: Vec<ControlRegion>,
}

/// Immutable revision chosen inside an admitted read transaction. This is not
/// a surrogate for control metadata: status, root and schema remain live-only.
struct ReadRevision {
    pin: IndexPin,
    key: String,
    /// Header rows of this immutable selected revision, read once per snapshot
    /// and shared by every document read in it.
    headers: Arc<SelectedHeaders>,
}
#[derive(Default)]
struct SelectedHeaders {
    producer_hash: OnceLock<String>,
}
impl ReadRevision {
    fn new(pin: IndexPin, key: String) -> Self {
        Self {
            pin,
            key,
            headers: Arc::default(),
        }
    }
    /// `selected_producer_hash`, computed once for this selected revision.
    /// A failure is not memoized: it fails the read and is recomputed if retried.
    fn producer_hash(&self, db: &Connection) -> Result<String> {
        if let Some(hash) = self.headers.producer_hash.get() {
            return Ok(hash.clone());
        }
        let hash = selected_producer_hash(db, self)?;
        Ok(self.headers.producer_hash.get_or_init(|| hash).clone())
    }
    fn current(db: &Connection) -> Result<Self> {
        let (generation, revision): (String, i64) = db.query_row(
            "SELECT index_generation,index_revision FROM index_metadata WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let pin: IndexPin = serde_json::from_value(
            serde_json::json!({"indexGeneration": generation, "indexRevision": revision}),
        )
        .context(SelectedIntegrity(
            "incompatible_index: stored pin decode failed".into(),
        ))?;
        Ok(Self::new(
            pin,
            format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision),
        ))
    }
}

/// Validated transport options for one pinned metrics projection.
#[derive(Debug, Clone)]
pub struct MetricsOptions {
    pub pin: Option<IndexPin>,
    pub role: String,
    pub workbench: bool,
    pub scope: String,
    pub source_set_id: Option<String>,
    pub language: Option<String>,
    pub path: Option<String>,
    pub sid: Option<String>,
    pub aggregate: String,
    /// Empty means all metrics; transport rejects an explicitly empty list.
    pub metrics: Vec<String>,
    pub top_cognitive: Option<usize>,
}
impl Default for MetricsOptions {
    fn default() -> Self {
        Self {
            pin: None,
            scope: "revision".into(),
            role: "all".into(),
            workbench: false,
            source_set_id: None,
            language: None,
            path: None,
            sid: None,
            aggregate: "sum".into(),
            metrics: vec![],
            top_cognitive: None,
        }
    }
}
impl MetricsOptions {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.role.as_str(), "production" | "all" | "test"),
            "invalid role"
        );
        ensure!(
            matches!(
                self.scope.as_str(),
                "revision" | "directory" | "file" | "declaration"
            ),
            "invalid scope"
        );
        ensure!(
            matches!(self.aggregate.as_str(), "sum" | "max" | "mean"),
            "invalid aggregate"
        );
        ensure!(
            self.metrics
                .iter()
                .all(|m| crate::metrics::METRIC_ORDER.contains(&m.as_str()))
                && self.metrics.iter().collect::<BTreeSet<_>>().len() == self.metrics.len(),
            "invalid metrics"
        );
        ensure!(
            self.top_cognitive
                .is_none_or(|count| (1..=10).contains(&count)),
            "invalid top cognitive count"
        );
        let located = self.scope != "revision";
        ensure!(
            self.source_set_id.is_some() == located && self.path.is_some() == located,
            "invalid metrics selector"
        );
        ensure!(
            self.language.is_some() == (self.scope == "file"),
            "invalid metrics language"
        );
        ensure!(
            self.sid.is_some() == (self.scope == "declaration"),
            "invalid metrics sid"
        );
        if let Some(path) = &self.path {
            ensure!(
                !path.starts_with('/')
                    && !path
                        .split('/')
                        .any(|s| s == "." || s == ".." || s.is_empty())
                    || (self.scope == "directory" && path.is_empty()),
                "invalid metrics path"
            );
        }
        Ok(())
    }
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsFileRow {
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub physical_lines: usize,
    pub file_role: DeclarationRole,
    pub role_counts: MetricsRoleCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excluded_reason: Option<&'static str>,
    pub declaration_count: usize,
    pub metrics: serde_json::Value,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsRoleCounts {
    pub production: usize,
    pub test: usize,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsDirectoryRow {
    pub source_set_id: String,
    pub path: String,
    pub declaration_count: usize,
    pub metrics: serde_json::Value,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsDeclarationRow {
    pub role: DeclarationRole,
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub sid: String,
    pub range: crate::native_evidence::Range,
    pub metrics: serde_json::Value,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsTopCognitiveRow {
    pub role: DeclarationRole,
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub sid: String,
    pub range: crate::native_evidence::Range,
    pub name: Option<String>,
    pub kind: String,
    pub metrics: MetricsTopCognitiveScore,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsTopCognitiveScore {
    pub cognitive: usize,
    pub cyclomatic: usize,
    pub max_nesting: usize,
}
const LAYER_EXCLUSIONS_WARNING: &str =
    "Rules unavailable; layer-based heat exclusions were not applied.";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsPayload {
    pub metrics_version: &'static str,
    pub role: String,
    pub workbench: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer_exclusions_warning: Option<&'static str>,
    pub index_generation: String,
    pub index_revision: u64,
    pub scope: String,
    pub aggregate: String,
    pub selected_metrics: Vec<String>,
    pub declarations: Vec<MetricsDeclarationRow>,
    pub files: Vec<MetricsFileRow>,
    pub directories: Vec<MetricsDirectoryRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_cognitive: Option<Vec<MetricsTopCognitiveRow>>,
}

/// One coherent native read snapshot retained until its owned result passes T03.
pub struct EvidenceResponse {
    store: Store,
    db: IndexConnection,
    fence: ReadFence,
    optional_recovery: OptionalRecovery,
}
/// Optional evidence cannot invalidate the required answer already being read.
/// Publish its repair request after that answer's fence, or on an early exit.
struct OptionalRecovery {
    store: Store,
    disposition: std::cell::Cell<Option<RecoveryDisposition>>,
}
impl OptionalRecovery {
    fn new(store: &Store) -> Self {
        Self {
            store: store.clone(),
            disposition: std::cell::Cell::new(None),
        }
    }
    fn record(&self, error: anyhow::Error) {
        let disposition = match recovery_class(&error) {
            RecoveryClass::Rebuild => RecoveryDisposition::Rebuild,
            RecoveryClass::RecreatePending => RecoveryDisposition::RecreatePending,
            RecoveryClass::Hard => return,
        };
        self.disposition
            .set(Some(self.disposition.get().map_or(disposition, |old| {
                if old as u8 > disposition as u8 {
                    old
                } else {
                    disposition
                }
            })));
    }
    fn publish(&self) {
        if let Some(disposition) = self.disposition.take() {
            self.store.mark_recovery(disposition);
        }
    }
}
impl Drop for OptionalRecovery {
    fn drop(&mut self) {
        self.publish();
    }
}
/// A response owns one validated SQLite snapshot until its root and selected
/// revision have been fenced at finish.
struct ReadFence(IndexPin);
impl ReadFence {
    fn verify(&self, store: &Store, db: Option<&Connection>) -> Result<()> {
        store.identity.verify()?;
        if let Some(db) = db {
            // Every read of a response shares one SQLite read transaction, so
            // the pin is the only state that can move underneath it. The
            // admitted head was fully validated once, at admission; the fence
            // does not repeat that validation (owner-accepted relaxation under
            // #261: reads trust what the indexer published).
            ensure!(
                ReadRevision::current(db)?.pin == self.0,
                "index_not_ready: selected snapshot changed"
            );
        }
        store.identity.verify()
    }
}
type TreeOverlays = Vec<(usize, String, usize)>;

// Adapt the read helpers' boolean progress seam without erasing the caller's
// original typed cancellation/deadline error. The request thread and the
// metrics workers share it: the caller's check runs on whichever thread asks,
// one at a time, and its first failure stops every later check.
struct MetricsReadCheck<'a> {
    check: Mutex<&'a mut (dyn FnMut() -> Result<()> + Send)>,
    failure: Mutex<Option<anyhow::Error>>,
    stopped: AtomicBool,
}
impl MetricsReadCheck<'_> {
    /// The caller's check; its failure is kept for `result` to report.
    fn expired(&self) -> bool {
        if self.stopped.load(Ordering::Acquire) {
            return true;
        }
        let mut check = self.check.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            return true;
        }
        match (*check)() {
            Ok(()) => false,
            Err(error) => {
                self.failure
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_insert(error);
                self.stopped.store(true, Ordering::Release);
                true
            }
        }
    }
    /// `expired` on every `every`-th call through `calls`, else only whether
    /// a check already failed. Concurrent workers poll the caller's check this
    /// way so they don't serialize on it; the poll interval is a few rows or
    /// parser steps.
    fn expired_every(&self, calls: &std::cell::Cell<u32>, every: u32) -> bool {
        let n = calls.get().wrapping_add(1);
        calls.set(n);
        if n.is_multiple_of(every) {
            self.expired()
        } else {
            self.stopped.load(Ordering::Acquire)
        }
    }
    fn check(&self) -> Result<()> {
        if self.expired() {
            return self.result(Err(anyhow::anyhow!("cancelled")));
        }
        Ok(())
    }
    /// The caller's check failure, if one happened, else `result`.
    fn result<T>(&self, result: Result<T>) -> Result<T> {
        if let Some(error) = self
            .failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            return Err(error);
        }
        result
    }
}

/// Metrics and file metadata from one fetched indexed document.
struct MetricsComputedDocument {
    computed: crate::metrics::Computed,
    physical_lines: usize,
    file_role: DeclarationRole,
}

/// Why one revision document did not produce metrics.
enum MetricsDocumentFailure {
    /// Reading the selected document failed; reported as a selected-read failure.
    Read(anyhow::Error),
    /// Any other failure.
    Other(anyhow::Error),
    /// The caller's check failed before this document's work.
    Stopped,
}

/// How often a metrics worker polls the caller's check, in progress steps.
/// A single-document request runs inline and checks at every step.
const METRICS_WORKER_CHECK_EVERY: u32 = 64;
/// At most this many documents are read into one chunk for the workers.
const METRICS_FETCH_DOCUMENTS: usize = 64;
/// The two overlapping metrics chunks together have at most this much
/// read-ahead charge. Each chunk has at most half the budget, except that a
/// document over half is computed alone. Deriving graph DTOs before charging
/// can also leave one fully fetched lookahead document carried across a chunk
/// boundary. Thus the held charge is pending + current + carried: at most
/// 64 MiB for the two ordinary chunks, plus one document within the existing
/// per-document source, ancillary, graph and F typed envelopes. This charge
/// counts derived graph bytes once; it is not a bound on decoded Rust RSS.
const METRICS_FETCH_BYTES: u64 = 64 * 1024 * 1024;

/// Test overrides for one metrics request on this thread.
#[cfg(test)]
#[derive(Clone, Copy)]
struct MetricsTestPlan {
    workers: usize,
    fetch_bytes: u64,
}
/// What one metrics request on this thread held between its reads and
/// its acceptance of each document.
#[cfg(test)]
#[derive(Debug, Default)]
struct MetricsFetchTrace {
    /// Each fetched document's read-ahead charge, in manifest order.
    charges: Vec<u64>,
    /// The most documents read and not yet accepted at once.
    max_resident_documents: usize,
    /// The most read-ahead charge held at once, including a fetched lookahead.
    max_resident_bytes: u64,
    /// (pending, current, carried) charges when a document crosses a boundary.
    boundaries: Vec<(u64, u64, u64)>,
}
#[cfg(test)]
thread_local! {
    static METRICS_TEST_PLAN: std::cell::Cell<Option<MetricsTestPlan>> =
        const { std::cell::Cell::new(None) };
    static METRICS_FETCH_TRACE: std::cell::RefCell<MetricsFetchTrace> =
        std::cell::RefCell::default();
}
#[cfg(test)]
fn trace_metrics_resident(documents: usize, bytes: u64) {
    METRICS_FETCH_TRACE.with_borrow_mut(|trace| {
        trace.max_resident_documents = trace.max_resident_documents.max(documents);
        trace.max_resident_bytes = trace.max_resident_bytes.max(bytes);
    });
}

impl EvidenceResponse {
    /// Materialize one captured-source architecture view in this admitted snapshot.
    pub fn architecture_response(
        &self,
        kind: &str,
        arguments: &serde_json::Value,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        ensure!(
            matches!(kind, "references" | "check" | "explain" | "prompt"),
            "invalid_request"
        );
        let pin = arguments
            .get("expectedBasis")
            .map(|value| {
                serde_json::from_value::<IndexPin>(value.clone())
                    .map_err(|_| anyhow::anyhow!("invalid_request: expectedBasis"))
            })
            .transpose()?;
        let paths: Vec<String> = arguments
            .get("paths")
            .map(|value| {
                serde_json::from_value(value.clone())
                    .map_err(|_| anyhow::anyhow!("invalid_request: paths"))
            })
            .transpose()?
            .unwrap_or_default();
        ensure!(
            paths
                .iter()
                .all(|path| crate::references::valid_path_glob(path)),
            "invalid_request: paths"
        );
        let sample_limit = arguments
            .get("limit")
            .map(|value| {
                value
                    .as_u64()
                    .filter(|limit| (1..=50).contains(limit))
                    .map(|limit| limit as usize)
                    .ok_or_else(|| anyhow::anyhow!("invalid_request: limit"))
            })
            .transpose()?
            .unwrap_or(5);
        check()?;
        let config = if kind == "references" {
            None
        } else {
            crate::rules::load(Path::new(self.store.workspace_root()))?
        };
        if matches!(kind, "check" | "explain") && config.is_none() {
            anyhow::bail!("rules_not_found");
        }
        let snapshot = self
            .architecture_snapshot(pin, check)
            .map_err(|error| self.store.report_index_failure(error))?;
        let references = crate::references::project(&snapshot, &paths, sample_limit, check)?;
        let mut payload = match kind {
            "references" => references.data,
            "prompt" => serde_json::json!({
                "rulesText": config.as_ref().map(|config| &config.text),
                "rulesFile": config.as_ref().map(crate::rules::descriptor),
                "references": references.data,
            }),
            "check" | "explain" => {
                let config = config.as_ref().expect("checked rules presence");
                let result = crate::rules::evaluate(config, &snapshot, &references, &paths, check)?;
                if kind == "explain" {
                    crate::rules::explain(
                        result,
                        arguments["rule"]
                            .as_str()
                            .context("invalid_request: rule")?,
                        arguments.get("target").context("invalid_request: target")?,
                        arguments.get("destination"),
                        check,
                    )?
                } else {
                    result
                }
            }
            _ => unreachable!(),
        };
        payload["indexGeneration"] = serde_json::json!(snapshot.pin.index_generation.to_string());
        payload["indexRevision"] = serde_json::json!(snapshot.pin.index_revision);
        check()?;
        Ok(payload)
    }

    /// Browser-only layer summary, references and metrics from this admitted read.
    pub fn architecture_view(
        &self,
        options: &MetricsOptions,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        options.validate()?;
        check()?;
        let config = crate::rules::load(Path::new(self.store.workspace_root()))?;
        let selected = self.selected(options.pin)?;
        let layers = config
            .as_ref()
            .map(crate::rules::view_layers)
            .unwrap_or_default();
        let mut payload = serde_json::json!({
            "indexGeneration": selected.pin.index_generation.to_string(),
            "indexRevision": selected.pin.index_revision,
            "state": if config.is_none() { "missing_rules" } else if layers.is_empty() { "no_layers" } else { "ready" },
            "rulesFile": config.as_ref().map(crate::rules::descriptor),
            "aggregate": options.aggregate, "metricsVersion": "metrics:v1",
            "role": options.role, "workbench": options.workbench, "evidenceTier": "syntax", "semanticComplete": false,
            "layers": layers, "modules": [], "edges": [], "references": null, "rules": null
        });
        if payload["state"] != "ready" {
            check()?;
            return Ok(payload);
        }
        let config = config.as_ref().expect("ready config");
        let snapshot = self
            .architecture_snapshot(Some(selected.pin), check)
            .map_err(|error| self.store.report_index_failure(error))?;
        let references = crate::references::project(&snapshot, &[], 5, check)?;
        let evaluated =
            crate::rules::evaluate_with_layers(config, &snapshot, &references, &[], check)?;
        let rules = evaluated.data;
        let assignments = evaluated.layers;
        let metrics = self.metrics_at_with_config(
            &MetricsOptions {
                pin: Some(snapshot.pin),
                role: options.role.clone(),
                workbench: options.workbench,
                aggregate: options.aggregate.clone(),
                ..MetricsOptions::default()
            },
            Some(config),
            true,
            check,
        )?;
        let files: BTreeMap<_, _> = metrics
            .files
            .iter()
            .map(|file| {
                (
                    (
                        file.source_set_id.as_str(),
                        file.language.as_str(),
                        file.path.as_str(),
                    ),
                    file,
                )
            })
            .collect();
        let documents: BTreeMap<_, _> = snapshot
            .documents
            .iter()
            .map(|doc| {
                (
                    (
                        doc.key.source_set_id.as_str(),
                        doc.key.language.as_str(),
                        doc.key.path.as_str(),
                    ),
                    doc,
                )
            })
            .collect();
        let mut modules = Vec::new();
        for module in references.data["modules"]
            .as_array()
            .context("store_unavailable: references modules")?
        {
            check()?;
            let target = &module["identity"];
            let key = (
                target["sourceSetId"]
                    .as_str()
                    .context("store_unavailable: module source set")?,
                target["language"]
                    .as_str()
                    .context("store_unavailable: module language")?,
                target["path"]
                    .as_str()
                    .context("store_unavailable: module path")?,
            );
            let file = files
                .get(&key)
                .context("store_unavailable: architecture metrics join")?;
            let doc = documents
                .get(&key)
                .context("store_unavailable: architecture source join")?;
            let mut row = module.clone();
            row["layer"] = serde_json::json!(
                assignments
                    .get(key.2)
                    .context("store_unavailable: architecture layer assignment absent")?
            );
            row["physicalLines"] = serde_json::json!(doc.file.text.lines().count());
            row["declarationCount"] = serde_json::json!(file.declaration_count);
            row["roleCounts"] = serde_json::json!(file.role_counts);
            row["metrics"] = file.metrics.clone();
            if let Some(reason) = file.excluded_reason {
                row["excludedReason"] = serde_json::json!(reason);
            }
            modules.push(row);
        }
        // Keep policy and exception contributions separate even on the same pair.
        type SummaryKey = (String, String, String, String, bool, Option<String>);
        let mut summaries: BTreeMap<SummaryKey, serde_json::Value> = BTreeMap::new();
        for (field, evaluated, role) in [
            ("production", "instances", "production"),
            ("tests", "testOnly", "test"),
        ] {
            let verdicts: BTreeMap<_, _> = rules[evaluated]
                .as_array()
                .context("store_unavailable: layer verdicts")?
                .iter()
                .filter(|row| row["rule"] == "$layers" && !row["destination"].is_null())
                .map(|row| {
                    (
                        (row["target"].to_string(), row["destination"].to_string()),
                        row,
                    )
                })
                .collect();
            for pair in references.data[field]
                .as_array()
                .context("store_unavailable: reference pairs")?
            {
                check()?;
                let verdict = verdicts
                    .get(&(pair["source"].to_string(), pair["target"].to_string()))
                    .context("store_unavailable: architecture verdict join")?;
                let (Some(source), Some(destination)) = (
                    verdict["sourceLayer"].as_str(),
                    verdict["destinationLayer"].as_str(),
                ) else {
                    continue;
                };
                if source == destination {
                    continue;
                }
                let status = verdict["status"]
                    .as_str()
                    .context("store_unavailable: layer status")?;
                let excepted = verdict["excepted"]
                    .as_bool()
                    .context("store_unavailable: layer exception")?;
                let reason = verdict["exceptionReason"].as_str().map(str::to_owned);
                let summary = summaries
                    .entry((
                        source.into(),
                        destination.into(),
                        role.into(),
                        status.into(),
                        excepted,
                        reason.clone(),
                    ))
                    .or_insert_with(|| {
                        serde_json::json!({
                            "sourceLayer": source, "destinationLayer": destination, "role": role,
                            "status": status, "excepted": excepted, "exceptionReason": reason,
                            "count": 0, "pairs": []
                        })
                    });
                summary["count"] = serde_json::json!(
                    summary["count"].as_u64().unwrap_or(0)
                        + pair["count"]
                            .as_u64()
                            .context("store_unavailable: reference count")?
                );
                summary["pairs"]
                    .as_array_mut()
                    .expect("constructed pairs")
                    .push(pair.clone());
            }
        }
        // Undrawn detail (sites, pairs, instances) is served by paged detail reads (#448).
        let (mut references, mut rules) = crate::architecture_details::summary(
            &references.data,
            &rules,
            &crate::architecture_details::Layers::new(&assignments, &rules),
        )?;
        drop(assignments);
        for value in [&mut references, &mut rules] {
            value["indexGeneration"] = serde_json::json!(snapshot.pin.index_generation.to_string());
            value["indexRevision"] = serde_json::json!(snapshot.pin.index_revision);
        }
        payload["modules"] = serde_json::json!(modules);
        payload["edges"] = serde_json::json!(summaries.into_values().collect::<Vec<_>>());
        payload["references"] = references;
        payload["rules"] = rules;
        check()?;
        Ok(payload)
    }

    /// One bounded page of Architecture detail at the summary's pin and rules hash.
    /// A different rules file fails `rules_conflict`; a stale pin fails like the summary.
    pub fn architecture_details(
        &self,
        query: &crate::architecture_details::Query,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<serde_json::Value> {
        check()?;
        let config = crate::rules::load(Path::new(self.store.workspace_root()))?
            .context("rules_conflict: rules file missing")?;
        ensure!(
            config.content_hash == query.rules_hash,
            "rules_conflict: rules changed since the summary"
        );
        let snapshot = self
            .architecture_snapshot(Some(query.pin), check)
            .map_err(|error| self.store.report_index_failure(error))?;
        let references = crate::references::project(&snapshot, &[], 5, check)?;
        let evaluated =
            crate::rules::evaluate_with_layers(&config, &snapshot, &references, &[], check)?;
        let layers = crate::architecture_details::Layers::new(&evaluated.layers, &evaluated.data);
        let page = crate::architecture_details::page(
            query,
            &references.data,
            &evaluated.data,
            &layers,
            check,
        )?;
        check()?;
        Ok(page)
    }

    /// Browser rule authoring validates the entire proposal against one admitted
    /// snapshot. Nothing here publishes or changes index state.
    pub(crate) fn validate_rules_edit(
        &self,
        pin: IndexPin,
        base: &crate::rules::Config,
        proposed: &crate::rules::Config,
        request: &crate::rules_edit::Request,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<()> {
        let snapshot = self
            .architecture_snapshot(Some(pin), check)
            .map_err(|error| self.store.report_index_failure(error))?;
        let references = crate::references::project(&snapshot, &[], 5, check)?;
        let evaluated =
            crate::rules::evaluate_with_layers(base, &snapshot, &references, &[], check)?;
        crate::rules_edit::validate_selection(request, &evaluated.data, &evaluated.layers, check)?;
        crate::rules::evaluate(proposed, &snapshot, &references, &[], check)?;
        Ok(())
    }

    pub(crate) fn rules_edit_inventory(&self, pin: IndexPin) -> Result<serde_json::Value> {
        self.selected(Some(pin))?;
        let config = crate::rules::load(Path::new(self.store.workspace_root()))?
            .context("not_found: rules file missing")?;
        crate::rules_edit::inventory(&config, pin)
    }

    /// Read each selected native document once; never reopen source from the checkout.
    fn architecture_snapshot(
        &self,
        pin: Option<IndexPin>,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<crate::references::Snapshot> {
        use crate::native_evidence::DocumentKey;
        check()?;
        let selected = self.selected(pin)?;
        let mut statement = self.db.prepare(
            "SELECT r.source_set_id,m.language,m.path FROM revision_documents m
             JOIN native_revisions r ON r.id=m.revision_id
             WHERE m.revision_id=?1 ORDER BY r.source_set_id,m.language,m.path",
        )?;
        let mut cursor = statement.query([&selected.key])?;
        let mut documents = Vec::new();
        let mut pending = Vec::new();
        let mut source_bytes = 0u64;
        let mut stored_bytes = 0u64;
        while let Some(row) = cursor.next()? {
            check()?;
            ensure!(
                pending.len() < 100_000,
                "bounded_work: architecture documents"
            );
            let key = DocumentKey {
                source_set_id: row.get(0)?,
                language: row.get(1)?,
                path: row.get(2)?,
            };
            let fetched = {
                let progress_failure = Mutex::new(None);
                let check = Mutex::new(&mut *check);
                let expired = || match (*check.lock().unwrap())() {
                    Ok(()) => false,
                    Err(error) => {
                        *progress_failure.lock().unwrap() = Some(error);
                        true
                    }
                };
                let header = self.store.fetch_selected_header(
                    &self.db,
                    &key.path,
                    &selected,
                    Some(&expired),
                );
                if let Some(error) = progress_failure.lock().unwrap().take() {
                    return Err(error);
                }
                let header = header?;
                source_bytes = source_bytes.saturating_add(header.bytes.max(0) as u64);
                stored_bytes = stored_bytes.saturating_add(header.stored_bytes);
                ensure!(
                    header.bytes <= 2 * 1024 * 1024
                        && source_bytes <= 256 * 1024 * 1024
                        && stored_bytes <= 512 * 1024 * 1024,
                    "bounded_work: architecture source"
                );
                let fetched =
                    self.store
                        .fetch_selected_raw(&self.db, header, &selected, Some(&expired));
                if let Some(error) = progress_failure.lock().unwrap().take() {
                    return Err(error);
                }
                fetched?
            };
            pending.push((key, std::sync::Mutex::new(Some(fetched))));
        }
        // SQLite reads stay on this connection; decoding needs no connection, so
        // it runs in parallel. The first decode failure in document order is reported.
        {
            let stopped = std::sync::atomic::AtomicBool::new(false);
            let progress_failure = Mutex::new(None);
            let check = Mutex::new(&mut *check);
            let expired = || {
                if stopped.load(std::sync::atomic::Ordering::Acquire) {
                    return true;
                }
                match (*check.lock().unwrap())() {
                    Ok(()) => false,
                    Err(error) => {
                        *progress_failure.lock().unwrap() = Some(error);
                        stopped.store(true, std::sync::atomic::Ordering::Release);
                        true
                    }
                }
            };
            let decoded = crate::parallel::map_ordered(
                &pending,
                crate::parallel::default_workers(),
                &stopped,
                |_, (_, decode)| {
                    let decode = decode
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take()
                        .expect("each fetched document is decoded once");
                    // Poll the shared request callback once per 256 rows per worker.
                    let polls = std::cell::Cell::new(0usize);
                    let poll = || {
                        let due = polls.get() == 0;
                        polls.set((polls.get() + 1) % 256);
                        if due {
                            expired()
                        } else {
                            stopped.load(std::sync::atomic::Ordering::Acquire)
                        }
                    };
                    decode(Some(&poll))
                },
            );
            if let Some(error) = progress_failure.lock().unwrap().take() {
                return Err(error);
            }
            for ((key, _), fetched) in pending.into_iter().zip(decoded) {
                let fetched = fetched.context("cancelled: architecture decode not started")??;
                selected_ensure!(
                    fetched.file.language == key.language,
                    "incompatible_index: architecture language mismatch"
                );
                documents.push(crate::references::DocumentInput {
                    key,
                    file: fetched.file,
                    declarations: fetched.declarations,
                    calls: fetched.calls,
                });
            }
        }
        // Retained manifests belong to this selected row and this admitted transaction.
        // Charge stored bytes before fetching JSON, and source bytes before hex decoding.
        let dependency_bytes: i64 = self.db.query_row(
            "SELECT length(CAST(retained_dependencies AS BLOB)) FROM native_revisions WHERE id=?1",
            [&selected.key],
            |row| row.get(0),
        )?;
        let dependency_bytes = u64::try_from(dependency_bytes).context(SelectedIntegrity(
            "incompatible_index: invalid retained dependencies length".into(),
        ))?;
        stored_bytes = stored_bytes.saturating_add(dependency_bytes);
        ensure!(
            stored_bytes <= 512 * 1024 * 1024,
            "bounded_work: architecture source"
        );
        check()?;
        let (source_set_id, native_revision_id, dependency_hash, retained): (String, String, String, String) = self.db.query_row(
            "SELECT source_set_id,native_revision_id,dependency_hash,retained_dependencies FROM native_revisions WHERE id=?1",
            [&selected.key], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        #[derive(serde::Deserialize)]
        #[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
        enum RetainedInput<'a> {
            Present {
                #[serde(borrow)]
                path: &'a str,
                #[serde(borrow)]
                bytes: &'a str,
                #[serde(rename = "byteLength")]
                byte_length: u64,
            },
            Absent {
                #[serde(borrow)]
                path: &'a str,
            },
        }
        let inputs: Vec<RetainedInput<'_>> = serde_json::from_str(&retained).context(
            SelectedIntegrity("incompatible_index: corrupt retained dependencies".into()),
        )?;
        check()?;
        let mut manifests = BTreeMap::new();
        for input in inputs {
            check()?;
            let path = match &input {
                RetainedInput::Present { path, .. } | RetainedInput::Absent { path } => *path,
            };
            if !matches!(path, "Cargo.toml" | "package.json") {
                continue;
            }
            let label = if path == "Cargo.toml" {
                "Cargo"
            } else {
                "package.json"
            };
            selected_ensure!(
                !manifests.contains_key(path),
                "incompatible_index: duplicate retained {} manifest",
                label
            );
            let (bytes, content_hash) = match input {
                RetainedInput::Absent { .. } => (None, None),
                RetainedInput::Present {
                    bytes, byte_length, ..
                } => {
                    source_bytes = source_bytes.saturating_add(byte_length);
                    ensure!(
                        byte_length <= 2 * 1024 * 1024 && source_bytes <= 256 * 1024 * 1024,
                        "bounded_work: architecture source"
                    );
                    selected_ensure!(
                        byte_length.checked_mul(2) == Some(bytes.len() as u64),
                        "incompatible_index: corrupt retained {} length",
                        label
                    );
                    check()?;
                    let bytes = hex::decode(bytes).context(SelectedIntegrity(format!(
                        "incompatible_index: corrupt retained {label} bytes"
                    )))?;
                    use sha2::{Digest, Sha256};
                    let hash = hex::encode(Sha256::digest(&bytes));
                    (Some(bytes), Some(hash))
                }
            };
            manifests.insert(
                path.to_owned(),
                crate::references::external::ManifestInput {
                    source_set_id: source_set_id.clone(),
                    native_revision_id: native_revision_id.clone(),
                    dependency_hash: dependency_hash.clone(),
                    path: path.into(),
                    bytes,
                    content_hash,
                },
            );
        }
        for path in ["Cargo.toml", "package.json"] {
            let label = if path == "Cargo.toml" {
                "Cargo"
            } else {
                "package.json"
            };
            selected_ensure!(
                manifests.contains_key(path),
                "incompatible_index: missing retained {} entry",
                label
            );
        }
        check()?;
        Ok(crate::references::Snapshot {
            pin: selected.pin,
            documents,
            manifests: manifests.into_values().collect(),
        })
    }

    /// Compute metrics from one selected revision in this response's read transaction.
    pub fn metrics_at(
        &self,
        options: &MetricsOptions,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<MetricsPayload> {
        options.validate()?;
        check()?;
        let (config, invalid_rules) = if options.workbench {
            match crate::rules::load(Path::new(self.store.workspace_root())) {
                Ok(config) => (config, false),
                Err(error) if crate::rules::diagnostic(&error).is_some() => (None, true),
                Err(error) => return Err(error),
            }
        } else {
            (None, false)
        };
        let mut payload = self.metrics_at_with_config(options, config.as_ref(), false, check)?;
        if invalid_rules {
            payload.layer_exclusions_warning = Some(LAYER_EXCLUSIONS_WARNING);
        }
        Ok(payload)
    }

    fn metrics_at_with_config(
        &self,
        options: &MetricsOptions,
        config: Option<&crate::rules::Config>,
        strict_rules: bool,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<MetricsPayload> {
        let progress = MetricsReadCheck {
            check: Mutex::new(check),
            failure: Mutex::new(None),
            stopped: AtomicBool::new(false),
        };
        progress.check()?;
        let selected = self.selected(options.pin)?;
        let selected_metrics: Vec<String> = crate::metrics::METRIC_ORDER
            .iter()
            .filter(|name| {
                options.metrics.is_empty() || options.metrics.iter().any(|m| m == **name)
            })
            .map(|name| (*name).to_owned())
            .collect();
        let names: Vec<&str> = selected_metrics.iter().map(String::as_str).collect();
        use crate::native_evidence::DocumentKey;
        let mut stmt = self.db.prepare(
            "SELECT r.source_set_id,m.language,m.path FROM revision_documents m
            JOIN native_revisions r ON r.id=m.revision_id
            WHERE m.revision_id=?1 ORDER BY r.source_set_id,m.language,m.path",
        )?;
        let mut documents = Vec::new();
        let mut cursor = stmt.query([&selected.key])?;
        while let Some(row) = cursor.next()? {
            progress.check()?;
            documents.push(DocumentKey {
                source_set_id: row.get(0)?,
                language: row.get(1)?,
                path: row.get(2)?,
            });
        }
        progress.check()?;
        // Existence is decided from the manifest, not from callable rows.
        let included: Vec<&DocumentKey> = documents
            .iter()
            .filter(|key| match options.scope.as_str() {
                "revision" => true,
                "directory" => {
                    options.source_set_id.as_ref() == Some(&key.source_set_id)
                        && options.path.as_ref().is_some_and(|path| {
                            path.is_empty() || key.path.starts_with(&format!("{path}/"))
                        })
                }
                "file" => {
                    options.source_set_id.as_ref() == Some(&key.source_set_id)
                        && options.language.as_ref() == Some(&key.language)
                        && options.path.as_ref() == Some(&key.path)
                }
                "declaration" => {
                    options.source_set_id.as_ref() == Some(&key.source_set_id)
                        && options.path.as_ref() == Some(&key.path)
                }
                _ => false,
            })
            .collect();
        if options.scope != "revision" && included.is_empty() {
            anyhow::bail!("not_found: metrics selector");
        }
        let mut layer_exclusions_warning = None;
        let hidden_layers = if options.workbench && options.role == "production" {
            if let Some(config) = config {
                match crate::rules::hidden_layer_paths(
                    config,
                    &included
                        .iter()
                        .map(|key| key.path.as_str())
                        .collect::<Vec<_>>(),
                    &mut || progress.check(),
                ) {
                    Ok(paths) => paths,
                    Err(error) if !strict_rules && crate::rules::diagnostic(&error).is_some() => {
                        layer_exclusions_warning = Some(LAYER_EXCLUSIONS_WARNING);
                        BTreeSet::new()
                    }
                    Err(error) => return Err(error),
                }
            } else {
                BTreeSet::new()
            }
        } else {
            BTreeSet::new()
        };
        let mut declarations = Vec::new();
        let mut files = Vec::new();
        let mut top_cognitive = options
            .top_cognitive
            .map(|_| Vec::<MetricsTopCognitiveRow>::new());
        let mut directories: BTreeMap<(String, String), crate::metrics::Accumulator> =
            BTreeMap::new();
        // Read source and native rows only from the selected version.
        // Every SQL read runs in order on this request's connection and snapshot.
        // Metrics run on workers, a chunk behind the
        // reads, and are accepted strictly in manifest order: the first failure
        // in that order is the one reported, and nothing after it is accepted.
        let workers = if included.len() > 1 {
            crate::parallel::default_workers()
        } else {
            1
        };
        #[cfg(test)]
        let workers = METRICS_TEST_PLAN
            .get()
            .map_or(workers, |plan| plan.workers.min(included.len().max(1)));
        let fetch_bytes = METRICS_FETCH_BYTES;
        #[cfg(test)]
        let fetch_bytes = METRICS_TEST_PLAN
            .get()
            .map_or(fetch_bytes, |plan| plan.fetch_bytes);
        let expired = || progress.expired();
        let check_every = if workers == 1 {
            1
        } else {
            METRICS_WORKER_CHECK_EVERY
        };
        let store = &self.store;
        let compute_document =
            |fetched: FetchedDocument,
             key: &DocumentKey|
             -> std::result::Result<MetricsComputedDocument, MetricsDocumentFailure> {
                let calls = std::cell::Cell::new(0);
                let expired = || progress.expired_every(&calls, check_every);
                // Metrics use the stored source and native rows as fetched.
                if fetched.file.language != key.language {
                    return Err(MetricsDocumentFailure::Other(
                        SelectedIntegrity("incompatible_index: metrics language mismatch".into())
                            .into(),
                    ));
                }
                if expired() {
                    return Err(MetricsDocumentFailure::Stopped);
                }
                let mut roots = fetched
                    .declarations
                    .iter()
                    .filter(|row| row.kind == "module" && row.ancestors.is_empty());
                let file_role = roots.next().map(|row| row.role);
                let Some(file_role) = file_role.filter(|_| roots.next().is_none()) else {
                    return Err(MetricsDocumentFailure::Read(
                        SelectedIntegrity("incompatible_index: metrics file module".into()).into(),
                    ));
                };
                let physical_lines = fetched.file.text.lines().count();
                let source = crate::metrics::SourceInput {
                    language: &key.language,
                    content_hash: &fetched.file.hash,
                    text: &fetched.file.text,
                };
                let cached = store
                    .metrics_memo
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .lookup(&source);
                crate::metrics::compute_with_cached(
                    cached,
                    source,
                    &fetched.declarations,
                    &fetched.calls,
                    &fetched.regions,
                    &mut || {
                        if expired() {
                            anyhow::bail!("cancelled")
                        }
                        Ok(())
                    },
                )
                .map(|computed| MetricsComputedDocument {
                    computed,
                    physical_lines,
                    file_role,
                })
                .map_err(MetricsDocumentFailure::Other)
            };
        let compute = |fetched: FetchedDocument, key: &DocumentKey| {
            #[cfg(test)]
            store.run_metrics_document_hook(&key.path, MetricsDocumentStage::Start);
            let outcome = compute_document(fetched, key);
            #[cfg(test)]
            store.run_metrics_document_hook(&key.path, MetricsDocumentStage::Done);
            outcome
        };
        let mut accept = |key: &DocumentKey,
                          outcome: Option<
            std::result::Result<MetricsComputedDocument, MetricsDocumentFailure>,
        >|
         -> Result<()> {
            let MetricsComputedDocument {
                computed,
                physical_lines,
                file_role,
            } = match outcome {
                Some(Ok(computed)) => computed,
                Some(Err(MetricsDocumentFailure::Read(error))) => {
                    return progress.result(Err(self.store.report_index_failure(error)));
                }
                Some(Err(MetricsDocumentFailure::Other(error))) => {
                    return progress.result(Err(error));
                }
                // A document a worker never started was skipped after a check failed.
                Some(Err(MetricsDocumentFailure::Stopped)) | None => {
                    return progress.result(Err(anyhow::anyhow!("cancelled")));
                }
            };
            let mut rows = computed.commit(
                &mut self
                    .store
                    .metrics_memo
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            )?;
            let mut role_counts = MetricsRoleCounts {
                production: 0,
                test: 0,
            };
            for row in &rows {
                progress.check()?;
                match row.role {
                    DeclarationRole::Production => role_counts.production += 1,
                    DeclarationRole::Test => role_counts.test += 1,
                }
            }
            // Workbench population only: stored roles, file identity and policy stay intact.
            // Validate and measure every file before excluding its callable contributions.
            let excluded_reason = if !options.workbench {
                None
            } else if key.path.starts_with("web/vendor/") {
                Some("vendored")
            } else if options.role == "production"
                && (key.path.starts_with("tests/fixtures/")
                    || key.path.starts_with("tools/synthetic-cohorts/templates/"))
            {
                Some("fixture")
            } else if hidden_layers.contains(&key.path) {
                Some("hidden_layer")
            } else {
                None
            };
            rows.retain(|row| {
                excluded_reason.is_none()
                    && (options.role == "all" || options.role == row.role.as_str())
            });
            let mut file_fold = crate::metrics::Accumulator::default();
            let mut parents = vec![String::new()];
            let mut directory = String::new();
            for segment in key
                .path
                .split('/')
                .collect::<Vec<_>>()
                .split_last()
                .map(|(_, parents)| parents)
                .unwrap_or(&[])
            {
                if !directory.is_empty() {
                    directory.push('/');
                }
                directory.push_str(segment);
                parents.push(directory.clone());
            }
            if options.scope == "directory" {
                let selected_path = options.path.as_deref().unwrap_or_default();
                parents.retain(|path| {
                    selected_path.is_empty()
                        || path == selected_path
                        || path.starts_with(&format!("{selected_path}/"))
                });
            }
            for path in &parents {
                directories
                    .entry((key.source_set_id.clone(), path.clone()))
                    .or_default();
            }
            for row in &rows {
                progress.check()?;
                file_fold.push(&row.metrics)?;
                for path in &parents {
                    directories
                        .get_mut(&(key.source_set_id.clone(), path.clone()))
                        .expect("inserted directory")
                        .push(&row.metrics)?;
                }
            }
            for row in rows {
                let selected_callable =
                    options.scope != "declaration" || options.sid.as_ref() == Some(&row.sid);
                if selected_callable
                    && let (Some(count), Some(top)) = (options.top_cognitive, &mut top_cognitive)
                {
                    top.push(MetricsTopCognitiveRow {
                        role: row.role,
                        source_set_id: row.source_set_id.clone(),
                        language: row.language.clone(),
                        path: row.path.clone(),
                        sid: row.sid.clone(),
                        range: row.range.clone(),
                        name: row.name.clone(),
                        kind: row.kind.clone(),
                        metrics: MetricsTopCognitiveScore {
                            cognitive: row.metrics.cognitive,
                            cyclomatic: row.metrics.cyclomatic,
                            max_nesting: row.metrics.max_nesting,
                        },
                    });
                    top.sort_by(|a, b| {
                        b.metrics.cognitive.cmp(&a.metrics.cognitive).then_with(|| {
                            (
                                &a.source_set_id,
                                &a.language,
                                &a.path,
                                a.range.start,
                                a.range.end,
                                &a.sid,
                            )
                                .cmp(&(
                                    &b.source_set_id,
                                    &b.language,
                                    &b.path,
                                    b.range.start,
                                    b.range.end,
                                    &b.sid,
                                ))
                        })
                    });
                    top.truncate(count);
                }
                if options.scope == "file" || (options.scope == "declaration" && selected_callable)
                {
                    let mut projected = serde_json::to_value(&row.metrics)?;
                    projected
                        .as_object_mut()
                        .expect("metric score object")
                        .retain(|name, _| selected_metrics.iter().any(|selected| selected == name));
                    declarations.push(MetricsDeclarationRow {
                        role: row.role,
                        source_set_id: row.source_set_id,
                        language: row.language,
                        path: row.path,
                        sid: row.sid,
                        range: row.range,
                        metrics: projected,
                    });
                }
            }
            if options.scope != "declaration" {
                files.push(MetricsFileRow {
                    source_set_id: key.source_set_id.clone(),
                    language: key.language.clone(),
                    path: key.path.clone(),
                    physical_lines,
                    file_role,
                    role_counts,
                    excluded_reason,
                    declaration_count: file_fold.count(),
                    metrics: file_fold.project(&options.aggregate, &names)?,
                });
            }
            Ok(())
        };
        std::thread::scope(|scope| -> Result<()> {
            type Fetched = std::result::Result<FetchedDocument, MetricsDocumentFailure>;
            let mut remaining = included.iter();
            let mut exhausted = false;
            // A document whose rows did not fit beside what is already held.
            let mut carried: Option<(&DocumentKey, FetchedDocument)> = None;
            let mut pending: Option<(
                Vec<&DocumentKey>,
                std::thread::ScopedJoinHandle<'_, Vec<Option<_>>>,
            )> = None;
            // The read-ahead charge of the chunk the workers hold, for the trace.
            #[cfg(test)]
            let mut pending_bytes = 0u64;
            let chunk_bytes = fetch_bytes / 2;
            loop {
                // Read the next chunk while the workers compute the previous one.
                // The two ordinary chunks together stay within the budget.
                // A document is charged its stored rows once fetched, so one
                // fully fetched document may be carried past that budget.
                // A document over half the budget is computed alone.
                let mut keys: Vec<&DocumentKey> = Vec::new();
                let mut fetched: Vec<std::sync::Mutex<Option<Fetched>>> = Vec::new();
                let mut bytes = 0u64;
                let mut alone = false;
                while keys.len() < METRICS_FETCH_DOCUMENTS {
                    let (key, item) = match carried.take() {
                        Some(carry) => carry,
                        None => {
                            if exhausted {
                                break;
                            }
                            let Some(&key) = remaining.next() else {
                                exhausted = true;
                                break;
                            };
                            let item = if progress.expired() {
                                Err(MetricsDocumentFailure::Stopped)
                            } else {
                                self.store
                                    .fetch_selected_header(
                                        &self.db,
                                        &key.path,
                                        &selected,
                                        Some(&expired),
                                    )
                                    .and_then(|header| {
                                        self.store.fetch_selected_rows(
                                            &self.db,
                                            header,
                                            &selected,
                                            Some(&expired),
                                        )
                                    })
                                    .map_err(MetricsDocumentFailure::Read)
                            };
                            match item {
                                Ok(item) => (key, item),
                                Err(failure) => {
                                    keys.push(key);
                                    fetched.push(std::sync::Mutex::new(Some(Err(failure))));
                                    exhausted = true;
                                    break;
                                }
                            }
                        }
                    };
                    let charge = item.stored_bytes;
                    // The newly fetched item is resident before deciding whether
                    // it fits: count pending + current + this lookahead even if
                    // pending is about to be joined and accepted.
                    #[cfg(test)]
                    trace_metrics_resident(
                        fetched.len() + pending.as_ref().map_or(0, |(keys, _)| keys.len()) + 1,
                        pending_bytes.saturating_add(bytes).saturating_add(charge),
                    );
                    let oversized = charge > chunk_bytes;
                    if !keys.is_empty() && (oversized || bytes.saturating_add(charge) > chunk_bytes)
                    {
                        #[cfg(test)]
                        METRICS_FETCH_TRACE.with_borrow_mut(|trace| {
                            trace.boundaries.push((pending_bytes, bytes, charge));
                        });
                        carried = Some((key, item));
                        break;
                    }
                    if oversized && pending.is_some() {
                        #[cfg(test)]
                        METRICS_FETCH_TRACE.with_borrow_mut(|trace| {
                            trace.boundaries.push((pending_bytes, bytes, charge));
                        });
                        carried = Some((key, item));
                        break;
                    }
                    keys.push(key);
                    fetched.push(std::sync::Mutex::new(Some(Ok(item))));
                    bytes = bytes.saturating_add(charge);
                    #[cfg(test)]
                    {
                        METRICS_FETCH_TRACE.with_borrow_mut(|trace| trace.charges.push(charge));
                    }
                    if oversized {
                        alone = true;
                        break;
                    }
                }
                if let Some((keys, handle)) = pending.take() {
                    #[cfg(test)]
                    {
                        pending_bytes = 0;
                    }
                    let outcomes = handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                    for (key, outcome) in keys.into_iter().zip(outcomes) {
                        accept(key, outcome)?;
                    }
                }
                if keys.is_empty() {
                    if carried.is_some() {
                        continue;
                    }
                    return Ok(());
                }
                let run = {
                    let compute = &compute;
                    let stopped = &progress.stopped;
                    let keys = keys.clone();
                    move || {
                        crate::parallel::map_ordered(&fetched, workers, stopped, |index, item| {
                            let item = item
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .take()
                                .expect("each fetched document is computed once");
                            item.and_then(|document| compute(document, keys[index]))
                        })
                    }
                };
                if workers == 1 || alone {
                    // Nothing else is read while this chunk is held.
                    for (key, outcome) in keys.into_iter().zip(run()) {
                        accept(key, outcome)?;
                    }
                } else {
                    #[cfg(test)]
                    {
                        pending_bytes = bytes;
                    }
                    pending = Some((keys, scope.spawn(run)));
                }
            }
        })?;
        if options.scope == "declaration" {
            if declarations.is_empty() {
                anyhow::bail!("not_found: metrics declaration");
            }
            selected_ensure!(
                declarations.len() == 1,
                "incompatible_index: ambiguous metrics declaration"
            );
        }
        declarations.sort_by(|a, b| {
            (
                &a.source_set_id,
                &a.language,
                &a.path,
                a.range.start,
                a.range.end,
                &a.sid,
            )
                .cmp(&(
                    &b.source_set_id,
                    &b.language,
                    &b.path,
                    b.range.start,
                    b.range.end,
                    &b.sid,
                ))
        });
        files.sort_by(|a, b| {
            (&a.source_set_id, &a.language, &a.path).cmp(&(&b.source_set_id, &b.language, &b.path))
        });
        let directory_rows = if matches!(options.scope.as_str(), "revision" | "directory") {
            directories
                .into_iter()
                .map(|((source_set_id, path), fold)| {
                    progress.check()?;
                    Ok(MetricsDirectoryRow {
                        source_set_id,
                        path,
                        declaration_count: fold.count(),
                        metrics: fold.project(&options.aggregate, &names)?,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            vec![]
        };
        progress.check()?;
        Ok(MetricsPayload {
            metrics_version: crate::metrics::VERSION,
            role: options.role.clone(),
            workbench: options.workbench,
            layer_exclusions_warning,
            index_generation: selected.pin.index_generation.to_string(),
            index_revision: selected.pin.index_revision,
            scope: options.scope.clone(),
            aggregate: options.aggregate.clone(),
            selected_metrics,
            declarations,
            files,
            directories: directory_rows,
            top_cognitive,
        })
    }
    pub(crate) fn report_index_failure(&self, error: anyhow::Error) -> anyhow::Error {
        self.store.report_index_failure(error)
    }
    pub(crate) fn report_optional_corruption(&self, error: anyhow::Error) {
        self.optional_recovery.record(error);
    }
    pub fn finish<T>(&self, value: T) -> Result<T> {
        let result = self.fence.verify(&self.store, Some(&self.db));
        self.optional_recovery.publish();
        result?;
        Ok(value)
    }
    pub fn status(&self) -> Result<IndexStatus> {
        self.store.read_status_for(&self.db, true)
    }
    pub fn validate_pin(&self, pin: IndexPin) -> Result<()> {
        self.selected(Some(pin))?;
        Ok(())
    }
    /// The response's selected revision: its admitted head, or `pin` after the
    /// usual conflict and expiry checks, without re-validating the index.
    fn selected(&self, pin: Option<IndexPin>) -> Result<ReadRevision> {
        self.store.read_revision_for(&self.db, pin, true)
    }
    pub fn source_at(
        &self,
        path: &str,
        expected: Option<IndexPin>,
    ) -> Result<Option<(IndexPin, SourceFile)>> {
        let selected = self.selected(expected)?;
        let source = self
            .store
            .selected_source_row_for(&self.db, path, &selected)?;
        Ok(source.map(|source| (selected.pin, source)))
    }
    pub fn query_view(&self, query: &ViewQuery) -> Result<Option<ViewResult>> {
        self.query_view_at(query, None)
    }
    pub fn query_view_at(
        &self,
        query: &ViewQuery,
        expected: Option<&IndexPin>,
    ) -> Result<Option<ViewResult>> {
        query.validate()?;
        self.store
            .query_view_in_for(&self.db, query, expected, true)
    }
    pub fn tree_metadata(
        &self,
        root: &Path,
        items: &mut [crate::file_tree::Entry],
    ) -> Result<(IndexPin, String)> {
        let (revision, workspace_root, overlays) =
            self.store.tree_metadata_in(&self.db, root, items, true)?;
        for (index, path, count) in overlays {
            items[index].indexed_path = Some(path);
            items[index].method_count = Some(count);
        }
        Ok((revision, workspace_root))
    }
    pub fn files_at(
        &self,
        expected: Option<IndexPin>,
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value> {
        self.store.files_in(&self.db, expected, offset, limit, true)
    }
    pub fn methods_at(
        &self,
        path: &str,
        expected: Option<IndexPin>,
    ) -> Result<Option<serde_json::Value>> {
        self.store.methods_in(&self.db, path, expected, true)
    }
    pub fn file_symbols_at(
        &self,
        path: &str,
        expected: IndexPin,
        offset: usize,
        limit: usize,
    ) -> Result<Option<crate::files_view::Outline>> {
        self.store
            .file_symbols_in(&self.db, path, expected, offset, limit, true)
    }
    pub fn syntax_matches_at(
        &self,
        request: &crate::files_view::SyntaxMatchesRequest,
    ) -> Result<Option<crate::files_view::SyntaxMatches>> {
        self.store.syntax_matches_in(&self.db, request, true)
    }
    pub fn sequence_at(
        &self,
        seed: &str,
        expected: IndexPin,
        show_all: bool,
    ) -> Result<Option<crate::behavior::SequenceView>> {
        self.store
            .sequence_in(&self.db, seed, expected, show_all, true)
    }
    pub fn classes_at(
        &self,
        path: Option<&str>,
        query: &str,
        expected: Option<IndexPin>,
        offset: usize,
        limit: usize,
    ) -> Result<crate::class_diagram::ClassPage> {
        self.store
            .classes_in(&self.db, path, query, expected, (offset, limit), true)
    }
    pub fn navigation_at(
        &self,
        request: &crate::navigation::NavigationRequest,
    ) -> Result<crate::navigation::NavigationResult> {
        self.store.navigation_in(&self.db, request, true)
    }
    pub fn class_diagram_at(
        &self,
        request: &crate::class_diagram::ClassDiagramRequest,
    ) -> Result<crate::class_diagram::ClassDiagram> {
        self.store.class_diagram_in(&self.db, request, true)
    }
    pub fn symbols_at(&self, query: &str, limit: usize) -> Result<(IndexPin, Vec<Symbol>)> {
        self.store.symbols_in(&self.db, query, limit, true)
    }
    pub fn graph(&self) -> Result<Graph> {
        let selected = self.selected(None)?;
        self.store.read_graph_for(&self.db, &selected)
    }
    pub fn symbol_at(
        &self,
        id: &str,
        expected_revision: Option<IndexPin>,
    ) -> Result<Option<(IndexPin, Symbol)>> {
        self.store.symbol_in(&self.db, id, expected_revision, true)
    }
    /// A predecessor permit is read-only. Mutations require a current strict
    /// read, with the root and publication fence checked before sidecar work.
    pub fn require_mutation_ready(&self) -> Result<()> {
        self.store.ensure_public_read_ready()?;
        self.finish(())
    }
    /// Ordinary reads release the snapshot before slow response assembly.
    /// A transitional exact-pin fence retains its predecessor snapshot; T03
    /// releases it after materialization so startup reconciliation may commit before finish.
    pub fn into_fence(self, policy: EvidenceFencePolicy) -> EvidenceFence {
        let Self {
            store,
            db,
            fence,
            optional_recovery,
        } = self;
        let snapshot_valid = fence.verify(&store, Some(&db)).is_ok();
        let snapshot = if matches!(policy, EvidenceFencePolicy::ExactPin(_)) {
            Some(db)
        } else {
            drop(db);
            None
        };
        EvidenceFence {
            store,
            fence,
            snapshot,
            snapshot_valid,
            policy,
            optional_recovery,
        }
    }
}
/// Unpinned responses need T03 only; pinned responses also require their exact pin.
#[derive(Clone, Copy)]
pub enum EvidenceFencePolicy {
    T03,
    ExactPin(IndexPin),
}
pub struct EvidenceFence {
    store: Store,
    fence: ReadFence,
    snapshot: Option<IndexConnection>,
    snapshot_valid: bool,
    policy: EvidenceFencePolicy,
    optional_recovery: OptionalRecovery,
}
impl EvidenceFence {
    pub fn finish<T>(&self, value: T) -> Result<T> {
        let result = self.finish_required(value);
        self.optional_recovery.publish();
        result
    }
    fn finish_required<T>(&self, value: T) -> Result<T> {
        self.fence.verify(&self.store, self.snapshot.as_deref())?;
        if !self.snapshot_valid {
            return Err(topology::IndexNotReady::new(
                "selected snapshot changed during materialization",
            )
            .into());
        }
        if let EvidenceFencePolicy::ExactPin(pin) = self.policy {
            let db = self
                .snapshot
                .as_ref()
                .expect("exact-pin fence retains snapshot");
            self.store.read_revision_for(db, Some(pin), true)?;
            self.fence.verify(&self.store, Some(db))?;
        }
        Ok(value)
    }
}

/// Refuse sidecars this open must not let SQLite act on. WAL and shared-memory
/// files are never supported, nor is a journal that is not a regular file
/// (symlink, FIFO or other type, refused before any byte is read). A writable
/// open refuses a regular rollback journal only if it may be hot: a cold one
/// (empty or zeroed header, left by a writer killed before its commit phase,
/// #220) never changed index.db, and SQLite ignores it and reuses it at the
/// next write. A hot journal is rolled back only by activation's recovery
/// under exclusive use (#122).
fn reject_sidecars(path: &Path, writable: bool) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        if suffix == "-journal" && !writable {
            continue;
        }
        let sidecar = path.with_file_name(format!(
            "{}{suffix}",
            path.file_name()
                .context("index filename missing")?
                .to_string_lossy()
        ));
        match std::fs::symlink_metadata(&sidecar) {
            Ok(meta) if suffix != "-journal" || !meta.file_type().is_file() => anyhow::bail!(
                "recovery_required: unsupported index sidecar: {}",
                sidecar.display()
            ),
            Ok(_) if journal_may_be_hot(&sidecar) => {
                anyhow::bail!("{HOT_INDEX_JOURNAL}: {}", sidecar.display())
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
// On POSIX, closing ANY fd for a SQLite database inode discards every
// fcntl lock held by this process for that inode, including another thread's
// SQLite writer. A check handle is therefore process-lifetime, not scoped to
// a read or a Store clone. One mutex serializes lookup/open/insert: a losing
// concurrent opener must never close a duplicate live-inode handle either.
// The fixed cap fails closed; never evict or close a live SQLite witness.
const MAX_RETAINED_SQLITE_WITNESSES: usize = 4096;
#[derive(Default)]
struct RetainedSqliteWitnesses {
    by_path: std::collections::HashMap<std::path::PathBuf, Vec<Arc<std::fs::File>>>,
    live: std::collections::HashMap<(std::path::PathBuf, u64, u64), usize>,
    count: usize,
    retired: std::collections::HashSet<std::path::PathBuf>,
}
static RETAINED_SQLITE_WITNESSES: std::sync::OnceLock<Mutex<RetainedSqliteWitnesses>> =
    std::sync::OnceLock::new();
fn sqlite_witnesses() -> &'static Mutex<RetainedSqliteWitnesses> {
    RETAINED_SQLITE_WITNESSES.get_or_init(|| Mutex::new(RetainedSqliteWitnesses::default()))
}

#[doc(hidden)]
pub fn retained_sqlite_witness_count_for_tests() -> usize {
    sqlite_witnesses()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .count
}

// The opener mutex serializes the last-connection close and retired witness
// removal. Closing any descriptor for a live SQLite inode releases all this
// process's fcntl locks, so skip the entire path if any connection or borrower
// still holds a handle. A later connection drop finishes deferred retirement.
fn clean_retired_sqlite_witness(registry: &mut RetainedSqliteWitnesses, path: &Path) {
    if !registry.retired.contains(path) {
        return;
    }
    let Some(files) = registry.by_path.get(path) else {
        registry.retired.remove(path);
        return;
    };
    let safe = files.iter().all(|file| {
        use std::os::unix::fs::MetadataExt;
        file.metadata().is_ok_and(|m| {
            registry
                .live
                .get(&(path.to_owned(), m.dev(), m.ino()))
                .copied()
                .unwrap_or(0)
                == 0
                && Arc::strong_count(file) == 1
        })
    });
    if safe {
        let files = registry.by_path.remove(path).expect("retired witness path");
        registry.count -= files.len();
        registry.retired.remove(path);
        drop(files);
    }
}

struct SqliteWitnessRetirement(Vec<std::path::PathBuf>);
impl Drop for SqliteWitnessRetirement {
    fn drop(&mut self) {
        let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
        for path in &self.0 {
            registry.retired.insert(path.clone());
            clean_retired_sqlite_witness(&mut registry, path);
        }
    }
}

// Every managed SQLite opener carries a live-inode registration until SQLite closes.
pub(crate) struct ProtectedSqliteConnection {
    db: Option<Connection>,
    key: (std::path::PathBuf, u64, u64),
}
impl Deref for ProtectedSqliteConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.db.as_ref().expect("closed SQLite connection")
    }
}
impl DerefMut for ProtectedSqliteConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.db.as_mut().expect("closed SQLite connection")
    }
}
impl Drop for ProtectedSqliteConnection {
    fn drop(&mut self) {
        let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
        drop(self.db.take());
        let live = registry
            .live
            .get_mut(&self.key)
            .expect("registered SQLite connection");
        *live -= 1;
        if *live == 0 {
            registry.live.remove(&self.key);
        }
        clean_retired_sqlite_witness(&mut registry, &self.key.0);
    }
}
pub(crate) fn protected_sqlite_open(
    path: &Path,
    flags: rusqlite::OpenFlags,
) -> Result<ProtectedSqliteConnection> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        path.is_absolute(),
        "unsafe_index: SQLite path must be absolute"
    );
    let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
    let named = std::fs::symlink_metadata(path)?;
    ensure!(
        named.is_file() && !named.file_type().is_symlink(),
        "unsafe_index: SQLite pathname changed"
    );
    let file = registry
        .by_path
        .get(path)
        .and_then(|files| {
            files.iter().find(|file| {
                file.metadata()
                    .is_ok_and(|m| (m.dev(), m.ino()) == (named.dev(), named.ino()))
            })
        })
        .context("unsafe_index: SQLite witness missing")?;
    let held = file.metadata()?;
    ensure!(
        (held.dev(), held.ino()) == (named.dev(), named.ino()),
        "unsafe_index: SQLite witness changed"
    );
    let db = Connection::open_with_flags(path, flags)?;
    let after = std::fs::symlink_metadata(path)?;
    ensure!(
        (after.dev(), after.ino()) == (named.dev(), named.ino()),
        "unsafe_index: SQLite pathname changed during open"
    );
    let key = (path.to_owned(), named.dev(), named.ino());
    *registry.live.entry(key.clone()).or_default() += 1;
    Ok(ProtectedSqliteConnection { db: Some(db), key })
}

/// Rename and release only the exact obsolete inode while holding verified EX
/// and the process-wide opener mutex. No managed connection can slip between
/// the live-count proof, rename and descriptor close.
fn replace_index_and_release_obsolete(
    old: IndexFileWitness,
    stage: &Path,
    leader: &topology::WriterGuard,
    roots: &topology::TopologyRoots,
    identity: &topology::WorkspaceIdentity,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let old_path = old.path.clone();
    let path = old_path.as_path();
    leader.verify_exclusive_use(&roots.index_use_lock(identity))?;
    ensure!(
        path == roots.index_db(identity),
        "unsafe_index: wrong obsolete index path"
    );
    old.verify()?;
    let held = old.file.metadata()?;
    let inode = (held.dev(), held.ino());
    let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
    ensure!(
        registry
            .live
            .get(&(path.to_owned(), inode.0, inode.1))
            .copied()
            .unwrap_or(0)
            == 0,
        "storage_busy: obsolete index has live SQLite connections"
    );
    let files = registry
        .by_path
        .get_mut(path)
        .context("unsafe_index: obsolete witness missing")?;
    let position = files
        .iter()
        .position(|file| file.metadata().is_ok_and(|m| (m.dev(), m.ino()) == inode))
        .context("unsafe_index: obsolete witness identity changed")?;
    ensure!(
        Arc::strong_count(&files[position]) == 2,
        "storage_busy: obsolete index witness still borrowed"
    );
    ensure!(
        registry.live.keys().all(|(named, _, _)| named != stage),
        "storage_busy: staged index still has live SQLite connections"
    );
    let staged_files = registry
        .by_path
        .get(stage)
        .context("unsafe_index: staged witness missing")?;
    ensure!(
        staged_files.len() == 1,
        "unsafe_index: unexpected staged witness count"
    );
    std::fs::rename(stage, path)?;
    drop(old);
    let files = registry
        .by_path
        .get_mut(path)
        .expect("verified obsolete witness");
    let file = files.remove(position);
    if files.is_empty() {
        registry.by_path.remove(path);
    }
    registry.count -= 1;
    drop(file);
    let staged = registry
        .by_path
        .remove(stage)
        .expect("verified staged witness");
    registry
        .by_path
        .entry(path.to_owned())
        .or_default()
        .extend(staged);
    Ok(())
}

/// Destructive GC holds the candidate's verified EX use lock and the opener
/// mutex across the final witness check and unlink. No protected connection or
/// borrowed witness may outlive the removed inode in this process.
pub(crate) fn gc_unlink_sqlite(
    paths: &[(std::path::PathBuf, (u64, u64))],
    exclusive: &topology::UseGuard,
    current: &topology::WorkspaceIdentity,
    leader: &topology::WriterGuard,
    after_first_unlink: &mut dyn FnMut() -> Result<()>,
) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
    let preflight = (|| -> Result<()> {
        for (path, inode) in paths {
            ensure!(
                matches!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("index.db" | "requests.db")
                ),
                "unsafe_index: GC SQLite path"
            );
            exclusive.verify_exclusive_path(
                &path
                    .parent()
                    .context("GC index parent missing")?
                    .with_extension("lock"),
            )?;
            let named = std::fs::symlink_metadata(path)?;
            ensure!(
                named.is_file()
                    && !named.file_type().is_symlink()
                    && (named.dev(), named.ino()) == *inode
                    && named.nlink() == 1
                    && named.mode() & 0o777 == 0o600,
                "unsafe_index: GC SQLite inode changed"
            );
            ensure!(
                registry.live.keys().all(|(name, _, _)| name != path),
                "storage_busy: GC SQLite connection still live"
            );
            let files = registry
                .by_path
                .get(path)
                .context("unsafe_index: GC witness missing")?;
            ensure!(
                files.len() == 1
                    && files[0]
                        .metadata()
                        .is_ok_and(|m| (m.dev(), m.ino()) == *inode)
                    && Arc::strong_count(&files[0]) == 1,
                "storage_busy: GC SQLite witness borrowed or changed"
            );
        }
        Ok(())
    })();
    if preflight.is_err() {
        // Busy, borrowed, replaced and unknown inodes are skips BEFORE any
        // unlink. The process-wide opener mutex still owns every witness.
        return Ok(false);
    }
    for (number, (path, _)) in paths.iter().enumerate() {
        // Keep proof current for every unlink, not only the first. Losing
        // authority after a partial deletion must fail rather than continue.
        current.verify()?;
        leader.verify()?;
        exclusive.verify()?;
        std::fs::remove_file(path)?;
        let files = registry.by_path.remove(path).expect("preflight GC witness");
        registry.count -= files.len();
        drop(files);
        if number == 0 {
            after_first_unlink()?;
        }
    }
    Ok(true)
}

/// Called only after GC has unlinked an exact managed SQLite inode while it
/// still holds the nonblocking verified EX index-use guard. Unrelated witnesses
/// (including another incarnation at the same name) are never released.
#[allow(dead_code)] // Also exercised by the retained SQLite witness lifecycle tests.
pub(crate) fn release_deleted_sqlite_witness(
    path: &Path,
    inode: (u64, u64),
    exclusive: &topology::UseGuard,
) -> Result<()> {
    release_deleted_sqlite_witness_kind(path, inode, exclusive, false)
}

fn release_deleted_sqlite_witness_kind(
    path: &Path,
    inode: (u64, u64),
    exclusive: &topology::UseGuard,
    failed_stage: bool,
) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    ensure!(
        path.is_absolute() && inode.0 != 0 && inode.1 != 0,
        "unsafe_index: unproved SQLite inode identity"
    );
    let name = path.file_name().and_then(|s| s.to_str());
    ensure!(
        if failed_stage {
            name.and_then(|s| s.strip_prefix("index.db.tmp-"))
                .is_some_and(|uuid| uuid::Uuid::parse_str(uuid).is_ok())
        } else {
            matches!(name, Some("index.db" | "requests.db"))
        },
        "unsafe_index: not an exact managed SQLite database"
    );
    let parent = path
        .parent()
        .context("unsafe_index: missing index directory")?;
    let lock = parent.with_extension("lock");
    exclusive.verify_exclusive_path(&lock)?;
    let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
    let named = match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
        Ok(named) => Some(named),
    };
    ensure!(
        registry
            .live
            .get(&(path.to_owned(), inode.0, inode.1))
            .copied()
            .unwrap_or(0)
            == 0,
        "storage_busy: deleted SQLite inode has live connections"
    );
    let files = registry
        .by_path
        .get_mut(path)
        .context("unsafe_index: deleted SQLite witness missing")?;
    let position = files
        .iter()
        .position(|file| file.metadata().is_ok_and(|m| (m.dev(), m.ino()) == inode))
        .context("unsafe_index: deleted SQLite witness identity changed")?;
    let held = files[position].metadata()?;
    ensure!(
        held.nlink() == 0,
        "unsafe_index: obsolete SQLite inode is still linked"
    );
    if named.is_some() {
        if failed_stage {
            return Err(ForeignStagedIndex.into());
        }
        anyhow::bail!("unsafe_index: SQLite pathname occupied; obsolete witness retained");
    }
    ensure!(
        Arc::strong_count(&files[position]) == 1,
        "storage_busy: deleted SQLite witness still borrowed"
    );
    let file = files.remove(position);
    if files.is_empty() {
        registry.by_path.remove(path);
    }
    registry.count -= 1;
    drop(file);
    Ok(())
}

/// Discard a disposable cache only after all local SQLite openers have closed.
/// Retained descriptors are released under the same opener registry, never
/// while a connection or an in-progress opener could still depend on its locks.
pub(crate) fn discard_sqlite_cache(path: &Path) -> Result<()> {
    let path = std::fs::canonicalize(path.parent().context("cache parent missing")?)?
        .join(path.file_name().context("cache filename missing")?);
    let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
    if registry.live.keys().any(|(named, _, _)| named == &path)
        || registry
            .by_path
            .get(&path)
            .is_some_and(|files| files.iter().any(|file| Arc::strong_count(file) != 1))
    {
        return Err(topology::StorageBusy.into());
    }
    let removed = (|| -> Result<()> {
        for suffix in ["-journal", "-wal", "-shm", ""] {
            let file = path.with_file_name(format!(
                "{}{suffix}",
                path.file_name().unwrap().to_string_lossy()
            ));
            match std::fs::remove_file(file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    })();
    registry.retired.insert(path.clone());
    clean_retired_sqlite_witness(&mut registry, &path);
    removed
}

pub(crate) fn retained_sqlite_file(
    path: &Path,
    writable: bool,
    create: bool,
    nonblocking: bool,
) -> Result<Arc<std::fs::File>> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut witnesses = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
    // Always check the current pathname, not just a previously held inode.
    // A replaced pathname gets another retained handle; the old one remains
    // open until process exit because its SQLite connection may still be live.
    if let Ok(named) = std::fs::symlink_metadata(path)
        && named.is_file()
        && !named.file_type().is_symlink()
        && let Some(files) = witnesses.by_path.get(path)
    {
        for file in files {
            if let Ok(held) = file.metadata()
                && (held.dev(), held.ino()) == (named.dev(), named.ino())
            {
                return Ok(file.clone());
            }
        }
    }
    // A staged inode may have been renamed to index.db. Reuse its exact
    // descriptor rather than open a second fd for the same SQLite inode.
    if let Ok(named) = std::fs::symlink_metadata(path)
        && named.is_file()
        && !named.file_type().is_symlink()
        && let Some(file) = witnesses.by_path.values().flatten().find(|file| {
            file.metadata()
                .is_ok_and(|m| (m.dev(), m.ino()) == (named.dev(), named.ino()))
        })
    {
        let file = file.clone();
        witnesses
            .by_path
            .entry(path.to_owned())
            .or_default()
            .push(file.clone());
        return Ok(file);
    }
    ensure!(
        witnesses.count < MAX_RETAINED_SQLITE_WITNESSES,
        "incompatible_index: verified SQLite file witness capacity reached"
    );
    let file = Arc::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .create(create)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | if nonblocking { libc::O_NONBLOCK } else { 0 })
            .open(path)?,
    );
    witnesses
        .by_path
        .entry(path.to_owned())
        .or_default()
        .push(file.clone());
    witnesses.count += 1;
    Ok(file)
}

/// Stage creation is single-flight with the same cache. Check capacity
/// BEFORE opening so no failure branch closes even a just-created SQLite fd.
fn create_retained_staged_sqlite_file(path: &Path) -> Result<Arc<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut witnesses = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
    ensure!(
        witnesses.count < MAX_RETAINED_SQLITE_WITNESSES,
        "incompatible_index: verified SQLite file witness capacity reached"
    );
    let file = Arc::new(
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?,
    );
    witnesses
        .by_path
        .entry(path.to_owned())
        .or_default()
        .push(file.clone());
    witnesses.count += 1;
    Ok(file)
}

fn verify_index_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::{FileExt, MetadataExt};
    let file = retained_sqlite_file(path, false, false, false)
        .context("incompatible_index: missing or unreadable database")?;
    let meta = file.metadata()?;
    let named = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o777 == 0o600
            && meta.nlink() == 1
            && named.is_file()
            && !named.file_type().is_symlink()
            && meta.dev() == named.dev()
            && meta.ino() == named.ino(),
        "unsafe_index: {}",
        path.display()
    );
    // A shared retained File has no mutable seek cursor. Positional reads
    // preserve exact SQLite-header validation without opening another fd.
    let mut header = [0u8; 20];
    let mut filled = 0;
    while filled < header.len() {
        match file.read_at(&mut header[filled..], filled as u64) {
            Ok(0) => return Err(ExceptionalIndexFormat.into()),
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error.into()),
        }
    }
    if &header[..16] != b"SQLite format 3\0" {
        return Err(ExceptionalIndexFormat.into());
    }
    // WAL (2,2) and other valid-header journaling modes are operational or
    // incompatible state, not permission to replace this inode.
    ensure!(
        header[18] == 1 && header[19] == 1,
        "incompatible_index: unsupported SQLite journaling mode"
    );
    Ok(())
}
// Treat dangling symlinks as existing, so a first open never replaces an unsafe path.
fn index_path_present(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn restore_index_journal(journal: &IndexFileWitness, backup: &Path, dir: &Path) -> Result<()> {
    journal.verify_at(backup)?;
    ensure!(
        !index_path_present(&journal.path)?,
        "unsafe_index: journal pathname reappeared"
    );
    std::fs::rename(backup, &journal.path)?;
    std::fs::File::open(dir)?.sync_all()?;
    journal.verify()
}

// A corrupt index can be witnessed without trusting its SQLite header or payload.
struct IndexFileWitness {
    path: std::path::PathBuf,
    file: Arc<std::fs::File>,
}
impl IndexFileWitness {
    fn open(path: &Path) -> Result<Self> {
        let file = retained_sqlite_file(path, false, false, true)?;
        let witness = Self {
            path: path.to_owned(),
            file,
        };
        witness.verify()?;
        Ok(witness)
    }
    fn verify(&self) -> Result<()> {
        self.verify_at(&self.path)
    }
    fn verify_at(&self, path: &Path) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let held = self.file.metadata()?;
        let named = std::fs::symlink_metadata(path)?;
        ensure!(
            held.is_file()
                && held.uid() == unsafe { libc::geteuid() }
                && held.mode() & 0o777 == 0o600
                && held.nlink() == 1
                && named.is_file()
                && !named.file_type().is_symlink()
                && held.dev() == named.dev()
                && held.ino() == named.ino(),
            "unsafe_index: protected index pathname changed"
        );
        Ok(())
    }
}
// The staged file is private to this attempt. On failure, only unlink our own inode.
#[derive(Debug)]
struct StagedIndex {
    path: std::path::PathBuf,
    file: Arc<std::fs::File>,
    published: bool,
}
impl StagedIndex {
    fn verify_path(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let held = self.file.metadata()?;
        let named = std::fs::symlink_metadata(&self.path)?;
        ensure!(
            held.is_file()
                && held.uid() == unsafe { libc::geteuid() }
                && held.mode() & 0o777 == 0o600
                && held.nlink() == 1
                && named.is_file()
                && !named.file_type().is_symlink()
                && held.dev() == named.dev()
                && held.ino() == named.ino(),
            "unsafe_index: staged pathname changed"
        );
        Ok(())
    }
}
// Aborted stages are explicitly unlinked and unwitnessed under verified EX by
// Store::cleanup_failed_staged_index. An unexpected drop leaves the temporary
// file on disk rather than silently creating a retained, deleted inode.
/// Match every cache object, including type, name, owning table, SQL, and
/// autoindexes. Unknown views and triggers cannot run during a publication.
fn validate_cache_shape(db: &Connection) -> Result<()> {
    let version: u32 = storage_result(db.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    ensure!(
        version <= DATABASE_SCHEMA_VERSION,
        "incompatible_index: unsupported newer schema cannot be rebaselined"
    );
    control_ensure!(
        version == DATABASE_SCHEMA_VERSION,
        "incompatible_index: unknown schema version"
    );
    let expected = expected_cache_objects()?;
    control_ensure!(
        &cache_objects(db)? == expected,
        "incompatible_index: unknown cache object type, name or shape"
    );
    Ok(())
}

type CacheObject = (String, String, String, Option<String>);

#[cfg(test)]
thread_local! {
    /// Expected-shape builds started on this thread.
    static EXPECTED_CACHE_SHAPE_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The expected shape is process-wide and immutable. A failed build is
/// returned to the caller and not cached, so the next call retries.
fn expected_cache_objects() -> Result<&'static Vec<CacheObject>> {
    static EXPECTED: std::sync::OnceLock<Vec<CacheObject>> = std::sync::OnceLock::new();
    if let Some(objects) = EXPECTED.get() {
        return Ok(objects);
    }
    #[cfg(test)]
    EXPECTED_CACHE_SHAPE_BUILDS.with(|count| count.set(count.get() + 1));
    let expected = Connection::open_in_memory()?;
    expected.execute_batch(CACHE_SCHEMA_V29)?;
    let built = cache_objects(&expected)?;
    Ok(EXPECTED.get_or_init(|| built))
}

fn cache_objects(db: &Connection) -> Result<Vec<CacheObject>> {
    // sqlite_master preparation also reads the schema. With journal_mode
    // DELETE it can meet the owner's write transaction before a follower
    // has elected or read its accepted request's DONE pin. Only direct
    // SQLite BUSY/LOCKED is typed for the coordinator's bounded retry.
    let mut statement = storage_result(
        db.prepare("SELECT type,name,tbl_name,sql FROM sqlite_master ORDER BY type,name"),
    )?;
    let rows = storage_result(statement.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    }))?;
    storage_result(rows.collect::<rusqlite::Result<Vec<_>>>())
}
#[cfg(test)]
mod cache_shape_once_tests {
    use super::*;

    fn current_schema_db() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(CACHE_SCHEMA_V29).unwrap();
        db.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)
            .unwrap();
        db
    }

    #[test]
    fn expected_shape_is_built_once_across_calls() {
        let first = expected_cache_objects().unwrap();
        let builds = EXPECTED_CACHE_SHAPE_BUILDS.with(std::cell::Cell::get);
        assert!(builds <= 1, "built {builds} times");
        for _ in 0..3 {
            validate_cache_shape(&current_schema_db()).unwrap();
        }
        assert!(std::ptr::eq(first, expected_cache_objects().unwrap()));
        assert_eq!(
            EXPECTED_CACHE_SHAPE_BUILDS.with(std::cell::Cell::get),
            builds
        );
    }

    #[test]
    fn drifted_schema_is_still_rejected() {
        for drift in [
            "ALTER TABLE revision_documents ADD COLUMN drift_extra TEXT",
            "CREATE INDEX drift_extra_idx ON revision_documents(path)",
            "DROP INDEX native_revisions_header_cover",
        ] {
            let db = current_schema_db();
            db.execute_batch(drift).unwrap();
            let failure = validate_cache_shape(&db).unwrap_err();
            assert!(
                failure
                    .to_string()
                    .contains("incompatible_index: unknown cache object type, name or shape"),
                "{drift}: {failure:#}"
            );
        }
    }
}

fn publication_second() -> Result<i64> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    ensure!(
        seconds <= 9_007_199_254_740_991,
        "invalid_supersession_time"
    );
    Ok(seconds as i64)
}

// SQLite's progress callback only tests whether a publisher has entered the
// gate's waiting state. The expensive queue/session probe belongs at unit
// boundaries, not on every 256 VM operations. Large rows must never be
// rolled back forever because a fixed wall-clock interval elapsed.
struct MaintenanceProgress<'a> {
    gate: &'a PublicationGate,
}
impl MaintenanceProgress<'_> {
    fn check(&self) -> bool {
        self.gate
            .waiting_published
            .load(std::sync::atomic::Ordering::Acquire)
            == 0
    }
}
unsafe extern "C" fn maintenance_progress_callback(arg: *mut std::ffi::c_void) -> libc::c_int {
    // SAFETY: the shared stack context outlives its registered SQLite handler.
    // RefCell permits only one active FnMut borrow, including reentrant calls.
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let context = unsafe { &*(arg as *const MaintenanceProgress<'_>) };
        if context.check() { 0 } else { 1 }
    }))
    .unwrap_or(1)
}
struct MaintenanceProgressGuard(*mut rusqlite::ffi::sqlite3);
impl Drop for MaintenanceProgressGuard {
    fn drop(&mut self) {
        // SAFETY: writer DB outlives this guard. Remove before context drops.
        unsafe { rusqlite::ffi::sqlite3_progress_handler(self.0, 0, None, std::ptr::null_mut()) };
    }
}
fn install_maintenance_progress(
    db: &Connection,
    context: &MaintenanceProgress<'_>,
) -> MaintenanceProgressGuard {
    // SAFETY: SQLite invokes this callback on the current connection's thread;
    // the guard removes it before `context` or `db` may be dropped.
    let handle = unsafe { db.handle() };
    unsafe {
        rusqlite::ffi::sqlite3_progress_handler(
            handle,
            256,
            Some(maintenance_progress_callback),
            (context as *const MaintenanceProgress<'_>)
                .cast_mut()
                .cast(),
        );
    }
    MaintenanceProgressGuard(handle)
}

#[cfg(test)]
mod maintenance_progress_ffi_tests;

fn maintenance_sqlite_busy(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SqliteContention>().is_some()
        || error
            .downcast_ref::<rusqlite::Error>()
            .is_some_and(|sqlite| {
                matches!(sqlite,
            rusqlite::Error::SqliteFailure(info, _) if matches!(info.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
            })
}

fn maintenance_contention(error: &anyhow::Error) -> bool {
    error.downcast_ref::<SqliteContention>().is_some()
        || error.downcast_ref::<topology::StorageBusy>().is_some()
        || error.downcast_ref::<rusqlite::Error>().is_some_and(|sqlite| matches!(sqlite,
            rusqlite::Error::SqliteFailure(info, _) if matches!(info.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked | rusqlite::ErrorCode::OperationInterrupted)))
}

fn cleanup_release_candidate(tx: &Connection, revision: &str, kind: &str) -> Result<()> {
    let (candidate, parent, reference, children, child_key, key) = match kind {
        "class" => (
            "native_release_candidate_classes",
            "class_projections",
            "cid",
            &["class_relations", "classes"][..],
            "cid",
            "cid",
        ),
        "graph" => (
            "native_release_candidate_graphs",
            "graph_projections",
            "gid",
            &["graph_display_labels", "graph_nodes"][..],
            "gid",
            "gid",
        ),
        "version" => (
            "native_release_candidate_versions",
            "document_versions",
            "vid",
            &[
                "native_version_call_regions",
                "native_version_references",
                "native_version_capability_gaps",
                "native_version_capability_families",
                "native_version_calls",
                "native_version_control_regions",
                "native_version_header_items",
                "native_version_parameters",
                "native_version_headers",
                "native_version_ancestor_signature_types",
                "native_version_own_signature_types",
                "native_version_declaration_ancestors",
                "native_version_search_cards",
                "native_version_declarations",
                "native_version_coverage_roles",
            ][..],
            "vid",
            "vid",
        ),
        _ => unreachable!("fixed cleanup kind"),
    };
    // A release unit can retire many small candidates. Referenced candidates
    // need just the indexed EXISTS probe and an un-journal; large child trees
    // retain the candidate while small committed DELETE batches resume later.
    for _ in 0..64 {
        let candidate_id: Option<i64> = tx
            .query_row(
                &format!(
                    "SELECT {key} FROM {candidate} WHERE revision_id=?1 ORDER BY {key} LIMIT 1"
                ),
                [revision],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = candidate_id else {
            return Ok(());
        };
        let still_referenced: bool = tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE {reference}=?1 LIMIT 1)"
            ),
            [id],
            |r| r.get(0),
        )?;
        if !still_referenced {
            // A projection absent from the manifest may still own another
            // projection. Journal its dependent before removing the parent.
            let dependent = match kind {
                "graph" => Some((
                    "class_projections",
                    "gid",
                    "native_release_candidate_classes",
                )),
                "version" => Some((
                    "graph_projections",
                    "vid",
                    "native_release_candidate_graphs",
                )),
                _ => None,
            };
            if let Some((table, field, queue)) = dependent {
                let child: Option<i64> = tx
                    .query_row(
                        &format!(
                            "SELECT {} FROM {table} WHERE {field}=?1 ORDER BY {} LIMIT 1",
                            if kind == "graph" { "cid" } else { "gid" },
                            if kind == "graph" { "cid" } else { "gid" }
                        ),
                        [id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(child) = child {
                    tx.execute(
                        &format!(
                            "INSERT OR IGNORE INTO {queue}(revision_id,{}) VALUES(?1,?2)",
                            if kind == "graph" { "cid" } else { "gid" }
                        ),
                        params![revision, child],
                    )?;
                    return Ok(());
                }
            }
            for child in children {
                // Self-referential declarations and control regions must be
                // removed together: a partial parent DELETE cannot commit with
                // references from still-present descendants.
                let self_referential = matches!(
                    *child,
                    "native_version_declarations" | "native_version_control_regions"
                );
                let changed = if self_referential {
                    tx.execute(&format!("DELETE FROM {child} WHERE {child_key}=?1"), [id])?
                } else if *child == "native_version_references" {
                    tx.execute(
                        "DELETE FROM native_version_references WHERE (vid,owner_syntax_id,ordinal) IN (
                         SELECT vid,owner_syntax_id,ordinal FROM native_version_references
                         WHERE vid=?1 ORDER BY owner_syntax_id,ordinal LIMIT 256)",
                        [id],
                    )?
                } else {
                    tx.execute(
                        &format!("DELETE FROM {child} WHERE rowid IN (SELECT rowid FROM {child} WHERE {child_key}=?1 LIMIT 256)"),
                        [id],
                    )?
                };
                if !self_referential && changed == 256 {
                    return Ok(());
                }
            }
            tx.execute(&format!("DELETE FROM {parent} WHERE {key}=?1"), [id])?;
        }
        tx.execute(
            &format!("DELETE FROM {candidate} WHERE revision_id=?1 AND {key}=?2"),
            params![revision, id],
        )?;
    }
    Ok(())
}

fn retention_due(now: i64, superseded_at: i64) -> bool {
    now.saturating_sub(superseded_at) >= 900
}

fn retention_clock_plausible(start: i64, wall: i64, elapsed: u64) -> bool {
    wall <= start
        .saturating_add(i64::try_from(elapsed).unwrap_or(i64::MAX))
        .saturating_add(5)
}

#[cfg(test)]
mod retention_clock_tests;

fn validate_supersessions(db: &Connection) -> Result<()> {
    let bad: i64 = db.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM native_revision_supersessions s
           LEFT JOIN native_revisions r ON r.id=s.revision_id
           CROSS JOIN index_metadata m
           WHERE r.id IS NULL OR typeof(s.superseded_at)!='integer'
             OR s.superseded_at NOT BETWEEN 0 AND 9007199254740991
             OR s.revision_id='pin:v1:'||m.index_generation||':'||m.index_revision
           UNION ALL
           SELECT 1 FROM native_revisions r CROSS JOIN index_metadata m
           WHERE r.id!='pin:v1:'||m.index_generation||':'||m.index_revision
             AND NOT EXISTS(SELECT 1 FROM revision_capture_inputs c
               WHERE c.revision_id=r.id AND c.input_key='__released:v1')
             AND NOT EXISTS(SELECT 1 FROM native_revision_supersessions s WHERE s.revision_id=r.id)
         )",
        [],
        |r| r.get(0),
    )?;
    control_ensure!(
        bad == 0,
        "incompatible_index: invalid revision supersession inventory"
    );
    Ok(())
}

fn stamp_predecessor(db: &Connection, old: &str, now: i64) -> Result<()> {
    ensure!(
        db.execute(
            "INSERT INTO native_revision_supersessions(revision_id,superseded_at,state) VALUES(?1,?2,'retained')",
            params![old, now],
        )? == 1,
        "incompatible_index: predecessor supersession missing"
    );
    Ok(())
}

thread_local! {
    static READ_DEADLINE: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

fn read_deadline() -> Option<Instant> {
    READ_DEADLINE.with(std::cell::Cell::get)
}

/// Run one read under a single deadline (#142). Every SQLite busy wait of a
/// read-only index connection opened on this thread inside `read` draws on
/// that one deadline, so waits cannot stack per statement; when it passes,
/// SQLite reports BUSY and the read fails with typed `storage_busy`. Nested
/// scopes keep the earlier deadline.
pub fn with_read_deadline<T>(deadline: Instant, read: impl FnOnce() -> T) -> T {
    struct Restore(Option<Instant>);
    impl Drop for Restore {
        fn drop(&mut self) {
            READ_DEADLINE.with(|cell| cell.set(self.0));
        }
    }
    let previous = read_deadline();
    let _restore = Restore(previous);
    let effective = previous.map_or(deadline, |earlier| earlier.min(deadline));
    READ_DEADLINE.with(|cell| cell.set(Some(effective)));
    read()
}

/// SQLite busy handler for read-scope connections: back off briefly while the
/// scope's deadline allows, then give up (BUSY). A connection used after its
/// scope ended falls back to a bounded wait (as `busy_timeout` would).
fn read_deadline_busy(attempt: i32) -> bool {
    if read_gate_blocked().is_some() {
        return false;
    }
    let pause = Duration::from_millis((1_u64 << attempt.clamp(0, 4) as u32).min(20));
    let remaining = match read_deadline() {
        Some(deadline) => deadline.saturating_duration_since(Instant::now()),
        None => Duration::from_secs(5).saturating_sub(pause * attempt.max(0) as u32),
    };
    if remaining.is_zero() {
        return false;
    }
    std::thread::sleep(pause.min(remaining));
    true
}

/// Page cache of a read-only index connection, in KiB (SQLite's negative form).
const READ_CACHE_KIB: i64 = -64 * 1024;

fn open_index_marker_probe(
    path: &Path,
    writable: bool,
    busy_wait: Duration,
) -> Result<ProtectedSqliteConnection> {
    use rusqlite::OpenFlags;
    reject_sidecars(path, writable)?;
    verify_index_file(path)?;
    // The writer attaches a batch stage by `file:` URI with `mode=ro`, so it must parse URIs. The
    // main path is absolute and never starts with `file:`, so it is always taken literally.
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut db = protected_sqlite_open(path, flags)?;
    // Inside a read scope a read-only connection's waits share the scope's one
    // deadline (#142); otherwise each SQLite call waits up to `busy_wait`.
    // A standard read outside a scope uses the same handler with its own 5 s
    // budget, so it too stops waiting at once on a spilled publication (#210).
    if !writable
        && !busy_wait.is_zero()
        && (read_deadline().is_some() || busy_wait == Duration::from_secs(5))
    {
        storage_result(db.busy_handler(Some(read_deadline_busy)))?;
    } else {
        storage_result(db.busy_timeout(busy_wait))?;
    }
    storage_result(db.pragma_update(None, "temp_store", "MEMORY"))?;
    storage_result(db.pragma_update(None, "foreign_keys", "ON"))?;
    if writable {
        storage_result(db.pragma_update(None, "synchronous", "FULL"))?;
    } else {
        storage_result(db.pragma_update(None, "query_only", "ON"))?;
        // One selected document read touches the same native pages several
        // times (row budgets, byte budgets, then decoding). A
        // page cache that holds a document's working set serves those repeats
        // from memory. Negative is KiB; pages are allocated only as used, and
        // the cache is freed with the per-request connection. No mmap: an
        // mmap'd I/O error would be a signal, not a typed error.
        storage_result(db.pragma_update(None, "cache_size", READ_CACHE_KIB))?;
        // Per-document read statements stay prepared for the request.
        db.set_prepared_statement_cache_capacity(64);
    }
    let mode: String = storage_result(db.pragma_query_value(None, "journal_mode", |r| r.get(0)))?;
    control_ensure!(mode == "delete", "incompatible_index: journal mode");
    Ok(db)
}

/// Inspect only the bounded control marker, never historical evidence or schema objects.
fn read_index_format_marker(db: &Connection) -> Result<IndexFormatMarker> {
    let mut statement = storage_result(db.prepare(
        "SELECT typeof(schema_version),schema_version,typeof(extractor_version),
                length(CAST(extractor_version AS BLOB)),
                CASE WHEN length(CAST(extractor_version AS BLOB))<=256
                     THEN extractor_version END
         FROM index_metadata LIMIT 2",
    ))?;
    let mut rows = storage_result(statement.query([]))?;
    let row = storage_result(rows.next())?.ok_or_else(|| {
        anyhow::Error::from(ControlIntegrity(
            "incompatible_index: missing index metadata marker".into(),
        ))
    })?;
    let schema_type: String = row.get(0)?;
    let schema: i64 = row.get(1)?;
    let extractor_type: String = row.get(2)?;
    let length: i64 = row.get(3)?;
    let extractor: Option<String> = row.get(4)?;
    control_ensure!(
        schema_type == "integer"
            && extractor_type == "text"
            && (0..=256).contains(&length)
            && extractor
                .as_ref()
                .is_some_and(|s| s.len() == length as usize)
            && storage_result(rows.next())?.is_none(),
        "incompatible_index: malformed index metadata marker"
    );
    Ok(IndexFormatMarker {
        schema_version: schema,
        extractor_version: extractor.expect("typed extractor checked"),
    })
}

fn open_index(path: &Path, writable: bool) -> Result<ProtectedSqliteConnection> {
    // Request-path schema/read admission waits through a normal writer commit.
    // The finite busy timeout is per SQLite call; the accepted FIFO request
    // keeps its durable ID if contention outlives this call.
    let db = open_index_marker_probe(path, writable, Duration::from_secs(5))?;
    let version: u32 = storage_result(db.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    ensure!(
        version <= DATABASE_SCHEMA_VERSION,
        "incompatible_index: unsupported newer schema cannot be rebaselined"
    );
    // Earlier derived schemas are disposable regardless of their metadata layout
    // or recorded root identity (Decision 0006 §2).
    if (1..DATABASE_SCHEMA_VERSION).contains(&version) {
        return Err(ObsoleteIndexFormat::EarlierSchema(version).into());
    }
    let marker = read_index_format_marker(&db)?;
    ensure!(
        marker.schema_version <= i64::from(DATABASE_SCHEMA_VERSION),
        "incompatible_index: unsupported newer schema cannot be rebaselined"
    );
    if marker.is_obsolete() {
        return Err(ObsoleteIndexFormat::Metadata(marker).into());
    }
    control_ensure!(
        version == DATABASE_SCHEMA_VERSION,
        "incompatible_index: unsupported disposable schema version"
    );
    storage_result(db.prepare(
        "SELECT index_generation,index_revision,indexed_at,stats,diagnostics FROM index_metadata",
    ))?;
    storage_result(db.prepare(
        "SELECT revision_id,language,path,vid,gid,cid,class_complete FROM revision_documents",
    ))?;
    storage_result(
        db.prepare("SELECT vid,id,language,path,content_hash,source_bytes FROM document_versions"),
    )?;
    storage_result(
        db.prepare("SELECT gid,vid,language,class_extraction_state,class_summary_payload FROM graph_projections"),
    )?;
    storage_result(db.prepare("SELECT gid,id,name,kind FROM graph_nodes"))?;
    storage_result(db.prepare("SELECT gid,id,label FROM graph_display_labels"))?;
    storage_result(db.prepare("SELECT cid,gid FROM class_projections"))?;
    storage_result(db.prepare("SELECT cid,id,payload FROM classes"))?;
    storage_result(db.prepare("SELECT cid,id,payload FROM class_relations"))?;
    validate_cache_shape(&db)?;
    Ok(db)
}

/// A fault in the unpublished scratch spool is not evidence that index.db is
/// corrupt. Do not retain a SQLite/JSON source in this wrapper: the live-index
/// recovery classifier examines the full error chain.
#[derive(Debug)]
struct NativeSpoolReadFailure(String);
impl std::fmt::Display for NativeSpoolReadFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "native_spool_failure: {}", self.0)
    }
}
impl std::error::Error for NativeSpoolReadFailure {}
fn native_spool_read_failure(error: anyhow::Error) -> anyhow::Error {
    NativeSpoolReadFailure(format!("{error:#}")).into()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecoveryClass {
    Rebuild,
    RecreatePending,
    Hard,
}
fn recovery_class(error: &anyhow::Error) -> RecoveryClass {
    if error.downcast_ref::<ExceptionalIndexFormat>().is_some() {
        return RecoveryClass::RecreatePending;
    }
    if error.downcast_ref::<ObsoleteIndexFormat>().is_some() {
        return RecoveryClass::RecreatePending;
    }
    if error.downcast_ref::<SelectedIntegrity>().is_some()
        || error.downcast_ref::<ControlIntegrity>().is_some()
    {
        return RecoveryClass::Rebuild;
    }
    if let Some(sqlite) = error.downcast_ref::<rusqlite::Error>() {
        return match sqlite {
            rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::Utf8Error(..)
            | rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::QueryReturnedNoRows
            | rusqlite::Error::QueryReturnedMoreThanOneRow => RecoveryClass::Rebuild,
            rusqlite::Error::SqliteFailure(info, _)
                if matches!(
                    info.code,
                    rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
                ) =>
            {
                RecoveryClass::RecreatePending
            }
            _ => RecoveryClass::Hard,
        };
    }
    RecoveryClass::Hard
}
#[derive(Debug)]
pub(crate) struct OptionalHintExpired;
impl std::fmt::Display for OptionalHintExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("optional hint work budget expired")
    }
}
impl std::error::Error for OptionalHintExpired {}

pub(crate) fn optional_hint_check(progress: Option<&dyn Fn() -> bool>) -> Result<()> {
    if progress.is_some_and(|expired| expired()) {
        return Err(OptionalHintExpired.into());
    }
    Ok(())
}

/// Opt-in, best-effort diagnostic stage; never carries IDs, paths, or pins.
pub(crate) fn index_diagnostic_stage(stage: &'static str) {
    if std::env::var("TRELLIS_INDEX_DIAGNOSTICS").as_deref() == Ok("1") {
        use std::io::Write as _;
        let stderr = std::io::stderr();
        let mut locked = stderr.lock();
        let _ = writeln!(locked, "index-phase {stage}");
    }
}
/// Only direct typed SQLite BUSY/LOCKED from terminal selected status reads.
/// A wrapped cause can belong to a higher-priority guard and is not retryable.
pub fn terminal_status_sqlite_contention(error: &anyhow::Error) -> bool {
    // Inspect only the outermost error. A direct rusqlite::Error may itself
    // expose a natural ffi::Error source; that is not an external guard context.
    let Some(direct) = error.chain().next() else {
        return false;
    };
    direct.downcast_ref::<SqliteContention>().is_some()
        || matches!(
            direct.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(info, _))
                if matches!(
                    info.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
        )
}

#[cfg(test)]
mod admit_once_tests;
#[cfg(test)]
mod published_admission_tests;
#[cfg(test)]
mod terminal_status_contention_tests;
#[cfg(test)]
thread_local! {
    /// Full status validations run on this thread; tests assert one per response.
    static STATUS_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A SQLite error from a full disk or a failed read or write.
fn storage_fault(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(info, _))
                if matches!(info.code,
                    rusqlite::ErrorCode::DiskFull | rusqlite::ErrorCode::SystemIoFailure))
    })
}

/// SQLite reported a full disk (SQLITE_FULL) while writing staged or published rows. The
/// request settles `storage_full`; the full cause stays in the error chain for diagnostics.
#[derive(Debug)]
pub struct StorageFull(());

impl std::fmt::Display for StorageFull {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("storage_full: database or disk is full")
    }
}

impl std::error::Error for StorageFull {}

/// Types a SQLITE_FULL anywhere in `error`'s chain as `StorageFull`; any other error is returned
/// unchanged.
pub(crate) fn typed_storage_full(error: anyhow::Error) -> anyhow::Error {
    let full = error.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(info, _))
                if info.code == rusqlite::ErrorCode::DiskFull)
    });
    if full && !error.is::<StorageFull>() {
        error.context(StorageFull(()))
    } else {
        error
    }
}

/// Publication contention only. A cached-claim mismatch or unresolved
/// completion also uses the `storage_busy` prefix, but is not a retryable
/// SQLite/lock conflict and must never be silently requeued.
pub(crate) fn transient_storage_contention(error: &anyhow::Error) -> bool {
    if error.chain().any(|cause| {
        cause.downcast_ref::<topology::StorageBusy>().is_some()
            || cause.downcast_ref::<SqliteContention>().is_some()
            || cause.downcast_ref::<RebuildInProgress>().is_some()
            || matches!(cause.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::SqliteFailure(info, _))
                    if matches!(info.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    }) {
        return true;
    }
    matches!(
        error.to_string().as_str(),
        "storage_busy: SQLite lock contention"
            | "storage_busy: index journal sidecar present"
            | "storage_busy: requests.db initialization in progress"
    )
}

/// A `storage_busy` error is never an accepted client's terminal failure,
/// even if its subtype is an invariant error rather than retryable contention.
pub(crate) fn nonterminal_storage_busy(error: &anyhow::Error) -> bool {
    transient_storage_contention(error) || error.to_string().starts_with("storage_busy")
}

/// A read-only CLI command may retry direct SQLite contention and exact
/// operational sidecar/initialization contention, but never a wrapped root,
/// incarnation, pin or selected-integrity guard or an invariant-busy string.
pub fn cli_read_retryable_contention(error: &anyhow::Error) -> bool {
    terminal_status_sqlite_contention(error)
        || error
            .chain()
            .next()
            .is_some_and(|direct| direct.downcast_ref::<topology::StorageBusy>().is_some())
        || (error.chain().count() == 1
            && matches!(
                error.to_string().as_str(),
                "storage_busy: index journal sidecar present"
                    | "storage_busy: requests.db initialization in progress"
            ))
}

/// A terminal typed error after the CLI read/admission wait expires. It is
/// distinct from a direct SQLite failure because sidecar contention also waits.
#[derive(Debug)]
pub struct CliReadBusy;

impl std::fmt::Display for CliReadBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("storage_busy: CLI read contention wait expired")
    }
}

impl std::error::Error for CliReadBusy {}

pub fn cli_read_busy_expired() -> anyhow::Error {
    CliReadBusy.into()
}

/// A transient SQLite writer-lock conflict, distinct from invariant-busy
/// control errors. The private constructor admits only exact SQLite BUSY/LOCKED.
#[derive(Debug)]
pub struct SqliteContention(());

impl std::fmt::Display for SqliteContention {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("storage_busy: SQLite lock contention")
    }
}

impl std::error::Error for SqliteContention {}

const HOT_INDEX_JOURNAL: &str = "recovery_required: hot index journal";

/// SQLite refused a read-only connection because `index.db-journal` looked hot.
#[derive(Debug)]
struct HotIndexJournal;

impl std::fmt::Display for HotIndexJournal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(HOT_INDEX_JOURNAL)
    }
}

impl std::error::Error for HotIndexJournal {}

/// A read-only hot-journal verdict can be a false positive (SQLite ticket
/// #3883, #183): SQLite saw a live writer's journal, then the writer rolled
/// back, deleting it and dropping RESERVED before SQLite looked for a RESERVED
/// holder, so the vanished journal read as hot. If the journal is no longer
/// hot by SQLite's own test (a non-empty first byte), the verdict was
/// contention: wait as a busy handler would and retry `read`, then report typed
/// contention once `busy_wait` (or the read scope's deadline) is spent. A
/// journal that is still hot keeps the verdict.
fn retry_stale_hot_journal<T>(
    index: &Path,
    busy_wait: Duration,
    mut read: impl FnMut() -> Result<T>,
) -> Result<T> {
    let deadline = Instant::now() + busy_wait;
    let mut attempt = 0;
    loop {
        match read() {
            Err(error)
                if error.downcast_ref::<HotIndexJournal>().is_some()
                    && !sqlite_journal_may_be_hot(index)? =>
            {
                let waited = if read_deadline().is_some() {
                    read_deadline_busy(attempt)
                } else {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let pause = Duration::from_millis((1_u64 << attempt.clamp(0, 4)).min(20));
                    std::thread::sleep(pause.min(remaining));
                    !remaining.is_zero()
                };
                if !waited {
                    return Err(SqliteContention(()).into());
                }
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Take the read snapshot of a deferred transaction on a read-only index
/// connection: its first read acquires SHARED, the one point where SQLite's
/// hot-journal check runs. Later reads in the transaction keep that lock.
/// `busy_wait` matches the connection's own SQLite busy wait.
fn begin_read_snapshot(db: &Connection, index: &Path, busy_wait: Duration) -> Result<()> {
    retry_stale_hot_journal(index, busy_wait, || {
        storage_result(db.pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0)))
            .map(|_| ())
    })
}

/// Whether `index.db-journal` may be hot: starting with SQLite's journal
/// magic. A journal shorter than the magic is cold. An otherwise unreadable
/// journal counts, so its recovery attempt reports a typed deferral instead of
/// skipping it.
fn hot_index_journal_present(index: &Path) -> Result<bool> {
    use std::io::Read;
    const MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];
    let journal = index.with_file_name("index.db-journal");
    match std::fs::symlink_metadata(&journal) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
        Ok(meta) if meta.len() == 0 => return Ok(false),
        Ok(_) => {}
    }
    let mut magic = [0u8; 8];
    match std::fs::File::open(&journal).and_then(|mut file| file.read_exact(&mut magic)) {
        Ok(()) => Ok(magic == MAGIC),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(_) => Ok(true),
    }
}

/// SQLite's hot test on `index.db-journal` without its lock check: present
/// with a non-zero first byte. An unreadable journal counts as hot.
fn sqlite_journal_may_be_hot(index: &Path) -> Result<bool> {
    Ok(journal_may_be_hot(
        &index.with_file_name("index.db-journal"),
    ))
}

/// SQLite's hot test on one rollback journal, without its lock check.
fn journal_may_be_hot(journal: &Path) -> bool {
    use std::io::Read;
    let mut first = [0u8; 1];
    match std::fs::File::open(journal).and_then(|mut file| file.read(&mut first)) {
        Ok(read) => read == 1 && first[0] != 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => true,
    }
}

fn storage_result<T>(result: rusqlite::Result<T>) -> Result<T> {
    match result {
        Err(rusqlite::Error::SqliteFailure(info, _))
            if info.extended_code == rusqlite::ffi::SQLITE_READONLY_ROLLBACK =>
        {
            Err(HotIndexJournal.into())
        }
        Err(rusqlite::Error::SqliteFailure(info, _))
            if matches!(
                info.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            match read_gate_blocked() {
                Some(rebuild) => Err(rebuild.into()),
                None => Err(SqliteContention(()).into()),
            }
        }
        other => Ok(other?),
    }
}
#[cfg(test)]
mod rebuild_in_progress_tests;
#[cfg(test)]
mod sqlite_contention_type_tests;

fn json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
fn check_cancel(cancel: &CancelFlag) -> Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "index publication cancelled"
    );
    Ok(())
}

fn validate_graph(graph: &Graph, cancel: &CancelFlag) -> Result<IndexStats> {
    let mut files = BTreeMap::new();
    for file in &graph.files {
        check_cancel(cancel)?;
        ensure!(
            !file.path.is_empty()
                && !Path::new(&file.path).is_absolute()
                && !file.path.split('/').any(|p| p == ".." || p.is_empty()),
            "invalid source path"
        );
        let mut lines = vec![0usize];
        lines.extend(
            file.text
                .bytes()
                .enumerate()
                .filter_map(|(i, b)| (b == b'\n').then_some(i + 1)),
        );
        ensure!(
            files.insert(file.path.as_str(), (file, lines)).is_none(),
            "duplicate file path"
        );
    }
    let span = |path: &str, range: &SourceRange| -> Result<()> {
        let (file, lines) = files
            .get(path)
            .context("graph references missing source file")?;
        ensure!(
            range.start_byte <= range.end_byte
                && range.end_byte <= file.text.len()
                && file.text.is_char_boundary(range.start_byte)
                && file.text.is_char_boundary(range.end_byte),
            "invalid source byte span"
        );
        for (byte, line, column) in [
            (range.start_byte, range.start_line, range.start_column),
            (range.end_byte, range.end_line, range.end_column),
        ] {
            let index = lines.partition_point(|start| *start <= byte) - 1;
            ensure!(
                line == index + 1 && column == byte - lines[index] + 1,
                "invalid source line/column span"
            );
        }
        Ok(())
    };
    let id = |id: &str| -> Result<()> {
        ensure!(
            !id.is_empty() && id.len() <= 8192 && !id.contains('\0'),
            "invalid graph id"
        );
        Ok(())
    };
    let mut nodes = BTreeMap::new();
    for node in &graph.nodes {
        check_cancel(cancel)?;
        id(&node.id)?;
        span(&node.path, &node.range)?;
        ensure!(
            nodes.insert(node.id.as_str(), node).is_none(),
            "duplicate node id"
        );
    }
    for node in &graph.nodes {
        if let Some(parent) = &node.parent {
            ensure!(nodes.contains_key(parent.as_str()), "dangling node parent");
        }
    }
    let mut regions = BTreeMap::new();
    for region in &graph.regions {
        check_cancel(cancel)?;
        id(&region.id)?;
        span(&region.path, &region.range)?;
        ensure!(
            nodes.contains_key(region.owner.as_str()),
            "dangling region owner"
        );
        ensure!(
            regions.insert(region.id.as_str(), region).is_none(),
            "duplicate region id"
        );
    }
    for region in &graph.regions {
        if let Some(parent) = &region.parent {
            ensure!(
                regions
                    .get(parent.as_str())
                    .is_some_and(|r| r.owner == region.owner),
                "dangling or foreign region parent"
            );
        }
    }
    let mut calls = BTreeSet::new();
    let mut stats = graph.stats.clone();
    stats.files = graph.files.len();
    stats.symbols = graph.nodes.len();
    stats.calls = graph.calls.len();
    stats.regions = graph.regions.len();
    stats.internal = 0;
    stats.external = 0;
    stats.unresolved = 0;
    stats.ambiguous = 0;
    ensure!(
        stats.parse_error_files <= stats.files,
        "parse error count exceeds source files"
    );
    for call in &graph.calls {
        check_cancel(cancel)?;
        id(&call.id)?;
        span(&call.path, &call.range)?;
        ensure!(calls.insert(call.id.as_str()), "duplicate call id");
        ensure!(
            nodes.contains_key(call.caller.as_str()),
            "dangling call caller"
        );
        // A call is a source-witnessed terminal occurrence, never a graph edge.
        // Production indexers project validated native IDs; shape-only Store
        // fixtures exercise graph integrity without replaying the extractor.
        stats.unresolved += 1;
        for region in &call.regions {
            ensure!(
                regions
                    .get(region.as_str())
                    .is_some_and(|r| r.owner == call.caller),
                "dangling or foreign call region"
            );
        }
    }
    Ok(stats)
}

fn class_metadata(db: &Connection, selected: &ReadRevision) -> Result<(Vec<String>, bool)> {
    let length: i64 = db.query_row(
        "SELECT length(CAST(class_warnings AS BLOB)) FROM native_revisions WHERE id=?1",
        [&selected.key],
        |r| r.get(0),
    )?;
    selected_ensure!(
        (0..=256 * 1024).contains(&length),
        "incompatible_index: class catalog byte budget exceeded"
    );
    let (warnings, mut truncated): (String, bool) = db.query_row(
        "SELECT class_warnings,class_truncated FROM native_revisions WHERE id=?1",
        [&selected.key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let warnings: Vec<String> = serde_json::from_str(&warnings).context(SelectedIntegrity(
        "incompatible_index: stored JSON decode failed".into(),
    ))?;
    let mut bytes = 0;
    let mut visible = Vec::new();
    for warning in warnings {
        bytes += serde_json::to_vec(&warning)?.len() + 1;
        if bytes > 256 * 1024 {
            truncated = true;
            visible.push("Further catalog warnings omitted by the presentation byte limit.".into());
            break;
        }
        visible.push(warning);
    }
    Ok((visible, truncated))
}
/// Read at most 64 KiB of class JSON into Rust. Large member arrays are
/// clipped in SQLite, without materializing their full strings in the API.
/// IDs, class metadata, and source ranges remain exact; only member arrays shrink.
fn presentation_class(
    store: &Store,
    db: &Connection,
    id: &str,
    selected: &ReadRevision,
) -> Result<Option<(crate::model::ClassDefinition, usize, bool)>> {
    use crate::class_diagram::CLASS_BYTES;
    let size: Option<i64> = db
        .query_row(
            "SELECT length(CAST(payload AS BLOB)) FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.cid=classes.cid WHERE classes.id=?1",
            params![id_codec::encode_sid(id), selected.key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|error| store.report_index_failure(error.into()))?;
    let Some(size) = size else {
        return Ok(None);
    };
    let valid: bool = db
        .query_row(
            "SELECT json_valid(payload) FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.cid=classes.cid WHERE classes.id=?1",
            params![id_codec::encode_sid(id), selected.key],
            |r| r.get(0),
        )
        .map_err(|error| store.report_index_failure(error.into()))?;
    if !valid {
        return Err(store.report_index_failure(
            SelectedIntegrity("incompatible_index: selected class JSON invalid".into()).into(),
        ));
    }
    if size <= CLASS_BYTES as i64 {
        let payload: String = db
            .query_row("SELECT payload FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.cid=classes.cid WHERE classes.id=?1", params![id_codec::encode_sid(id), selected.key], |r| {
                r.get(0)
            })
            .map_err(|error| store.report_index_failure(error.into()))?;
        let class = serde_json::from_str(&payload)
            .context(SelectedIntegrity(
                "incompatible_index: stored JSON decode failed".into(),
            ))
            .map_err(|error| store.report_index_failure(error))?;
        return Ok(Some((class, payload.len(), false)));
    }
    let clip_shape: bool = db
        .query_row(
            "SELECT json_type(payload,'$.fields')='array'
                AND json_type(payload,'$.methods')='array'
                AND NOT EXISTS(SELECT 1 FROM json_each(payload,'$.fields') WHERE type!='object')
                AND NOT EXISTS(SELECT 1 FROM json_each(payload,'$.methods') WHERE type!='object')
             FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.cid=classes.cid WHERE classes.id=?1",
            params![id_codec::encode_sid(id), selected.key],
            |r| r.get(0),
        )
        .map_err(|error| store.report_index_failure(error.into()))?;
    if !clip_shape {
        return Err(store.report_index_failure(
            SelectedIntegrity("incompatible_index: selected class member JSON invalid".into())
                .into(),
        ));
    }
    for count in [32, 16, 8, 4, 2, 1, 0] {
        let payload: Option<String> = db.query_row("WITH clipped AS (
            SELECT json_set(payload,
                '$.fields',(SELECT json_group_array(json(value)) FROM json_each(classes.payload,'$.fields') WHERE key < ?2),
                '$.methods',(SELECT json_group_array(json(value)) FROM json_each(classes.payload,'$.methods') WHERE key < ?2),
                '$.truncated',json('true')) AS payload FROM classes JOIN revision_documents m ON m.revision_id=?4 AND m.cid=classes.cid WHERE classes.id=?1)
            SELECT CASE WHEN length(CAST(payload AS BLOB)) <= ?3 THEN payload ELSE NULL END FROM clipped",
            params![id_codec::encode_sid(id), count, CLASS_BYTES as i64, selected.key], |r| r.get(0))
            .map_err(|error| store.report_index_failure(error.into()))?;
        if let Some(payload) = payload {
            let class = serde_json::from_str(&payload)
                .context(SelectedIntegrity(
                    "incompatible_index: stored JSON decode failed".into(),
                ))
                .map_err(|error| store.report_index_failure(error))?;
            return Ok(Some((class, payload.len(), true)));
        }
    }
    // Pathological non-member metadata cannot fit without changing identity.
    Ok(None)
}
fn resolve_class_id(
    store: &Store,
    db: &Connection,
    id: &str,
    selected: &ReadRevision,
) -> Result<String> {
    use crate::class_diagram::InvalidRequest;
    let mut current = id.to_owned();
    let mut seen = BTreeSet::new();
    let mut document: Option<SelectedDocument> = None;
    for depth in 0..64 {
        ensure!(
            seen.insert(current.clone()),
            InvalidRequest("The selected symbol has a cyclic class ancestry.")
        );
        if depth > 0 {
            ensure!(
                document
                    .as_ref()
                    .is_some_and(|doc| doc.nodes.iter().any(|node| node.id == current)),
                InvalidRequest("Choose an indexed Java or Python class or method.")
            );
        }
        if db.query_row(
            "SELECT EXISTS(SELECT 1 FROM classes c JOIN revision_documents m ON m.revision_id=?2 AND m.cid=c.cid WHERE c.id=?1)",
            params![id_codec::encode_sid(&current), selected.key],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(current);
        }
        if document.is_none() {
            let path: Option<String> = db.query_row(
                "SELECT m.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1",
                params![id_codec::encode_sid(&current), selected.key], |r| r.get(0),
            ).optional().map_err(|error| store.report_index_failure(error.into()))?;
            document = path
                .map(|path| store.selected_graph_document_for(db, &path, selected))
                .transpose()?;
        }
        let symbol = document
            .as_ref()
            .and_then(|doc| doc.nodes.iter().find(|node| node.id == current))
            .ok_or(InvalidRequest(
                "Choose an indexed Java or Python class or method.",
            ))?;
        ensure!(
            depth > 0 || matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function),
            InvalidRequest("Choose an indexed Java or Python class or method.")
        );
        current = symbol.parent.clone().ok_or(InvalidRequest(
            "The selected method has no supported enclosing class.",
        ))?;
    }
    Err(InvalidRequest("The selected symbol exceeds the class ancestry limit.").into())
}

fn resolve_class(
    store: &Store,
    db: &Connection,
    id: &str,
    selected: &ReadRevision,
) -> Result<(crate::model::ClassDefinition, bool)> {
    let id = resolve_class_id(store, db, id, selected)?;
    let (class, _, clipped) = presentation_class(store, db, &id, selected)?.ok_or(
        crate::class_diagram::InvalidRequest("Class metadata exceeds the presentation byte limit."),
    )?;
    Ok((class, clipped))
}

fn v8_id(prefix: &str, domain: &[u8], value: serde_json::Value) -> String {
    format!(
        "{prefix}:{}",
        crate::native_ids::digest(domain, &crate::native_ids::canonical(&value))
    )
}
#[derive(Clone)]
struct V8DocumentProjection {
    version_id: String,
    graph_id: String,
    class_id: String,
    graph_hash: String,
    class_hash: String,
}

// Group immutable publication facts once, outside the IMMEDIATE writer transaction.
// The original slices retain source order; projection hashing applies its own ID order.
#[derive(Default)]
struct PublicationRows<'a> {
    nodes: Vec<&'a Symbol>,
    calls: Vec<&'a CallSite>,
    regions: Vec<&'a ControlRegion>,
    classes: Vec<&'a crate::model::ClassDefinition>,
    relations: Vec<&'a crate::model::ClassRelation>,
    native_declarations: Vec<&'a crate::native_evidence::Declaration>,
    native_calls: Vec<&'a crate::native_evidence::Call>,
    native_regions: Vec<&'a crate::native_evidence::ControlRegion>,
    native_references: Vec<&'a crate::native_evidence::Reference>,
    capability_report: Option<&'a crate::native_evidence::CapabilityReport>,
}
fn publication_native_witness(
    document: &crate::native_evidence::Document,
    producer: &crate::native_evidence::Producer,
    coverage: &crate::native_evidence::Coverage,
    grouped: &PublicationRows<'_>,
) -> Result<String> {
    crate::native_evidence::document_witness(
        document,
        producer,
        coverage,
        grouped.native_declarations.iter().copied(),
        grouped.native_calls.iter().copied(),
        grouped.native_regions.iter().copied(),
        grouped.native_references.iter().copied(),
        grouped
            .capability_report
            .context("native capability report missing")?,
    )
}

#[cfg(test)]
#[test]
fn publication_witness_requires_capability_report() {
    use crate::native_evidence::{Coverage, Document, DocumentKey, Producer};

    let document = Document {
        key: DocumentKey {
            source_set_id: "source-set".into(),
            language: "javascript".into(),
            path: "a.js".into(),
        },
        revision_id: "revision".into(),
        content_hash: "a".repeat(64),
        byte_length: 0,
    };
    let producer = Producer {
        id: crate::native_evidence::PRODUCER.into(),
        version: crate::native_evidence::NATIVE_VERSION.into(),
        executable_hash: "e".repeat(64),
        kind: "native".into(),
        languages: vec!["javascript".into()],
        position_encoding: "utf8".into(),
    };
    let coverage = Coverage {
        producer_id: producer.id.clone(),
        language: document.key.language.clone(),
        source_set_id: document.key.source_set_id.clone(),
        document_path: document.key.path.clone(),
        revision_id: document.revision_id.clone(),
        requested: true,
        selected: true,
        state: "complete".into(),
        supported_roles: Vec::new(),
        observed_roles: Vec::new(),
        diagnostic: None,
    };
    let error =
        publication_native_witness(&document, &producer, &coverage, &PublicationRows::default())
            .unwrap_err();
    assert_eq!(error.to_string(), "native capability report missing");
}

fn publication_rows<'a>(
    graph: &'a Graph,
    native: &'a crate::native_evidence::Artifact,
    classes: &'a crate::classes::Catalog,
) -> Result<BTreeMap<&'a str, PublicationRows<'a>>> {
    let mut rows = BTreeMap::<&str, PublicationRows>::new();
    for file in &graph.files {
        ensure!(
            rows.insert(&file.path, PublicationRows::default())
                .is_none(),
            "duplicate graph path"
        );
    }
    for node in &graph.nodes {
        rows.get_mut(node.path.as_str())
            .context("graph node path missing")?
            .nodes
            .push(node);
    }
    for call in &graph.calls {
        rows.get_mut(call.path.as_str())
            .context("graph call path missing")?
            .calls
            .push(call);
    }
    for region in &graph.regions {
        rows.get_mut(region.path.as_str())
            .context("graph region path missing")?
            .regions
            .push(region);
    }
    let mut owners = BTreeMap::new();
    for class in &classes.classes {
        rows.get_mut(class.symbol.path.as_str())
            .context("class path missing")?
            .classes
            .push(class);
        ensure!(
            owners
                .insert(class.symbol.id.as_str(), class.symbol.path.as_str())
                .is_none(),
            "duplicate class owner"
        );
    }
    for relation in &classes.relations {
        let path = owners
            .get(relation.owner.as_str())
            .context("class relation owner missing")?;
        rows.get_mut(path)
            .context("class relation path missing")?
            .relations
            .push(relation);
    }
    for decl in &native.declarations {
        rows.get_mut(decl.document.path.as_str())
            .context("native declaration path missing")?
            .native_declarations
            .push(decl);
    }
    for call in &native.calls {
        rows.get_mut(call.document.path.as_str())
            .context("native call path missing")?
            .native_calls
            .push(call);
    }
    for region in &native.control_regions {
        rows.get_mut(region.document.path.as_str())
            .context("native control path missing")?
            .native_regions
            .push(region);
    }
    for reference in &native.references {
        rows.get_mut(reference.document.path.as_str())
            .context("native reference path missing")?
            .native_references
            .push(reference);
    }
    for report in &native.capability_reports {
        let grouped = rows
            .get_mut(report.document.path.as_str())
            .context("native capability path missing")?;
        ensure!(
            grouped.capability_report.replace(report).is_none(),
            "duplicate native capability report"
        );
    }
    Ok(rows)
}

// Catalog composition has global limits and warnings, while each SQL family
// and projection hash uses only rows owned by the current document.
fn stream_document_classes(
    catalog: &crate::classes::Catalog,
    path: &str,
) -> crate::classes::Catalog {
    // CatalogComposer::finish sorts both vectors by document path before their
    // within-document key (the same order as Catalog::compose). Keep that
    // invariant if either composer changes: partition_point requires it.
    // Find that path's range without rescanning the
    // bounded workspace catalog for each streamed document.
    let first_class = catalog
        .classes
        .partition_point(|c| c.symbol.path.as_str() < path);
    let first_relation = catalog
        .relations
        .partition_point(|r| r.path.as_str() < path);
    crate::classes::Catalog {
        classes: catalog.classes[first_class..]
            .iter()
            .take_while(|c| c.symbol.path == path)
            .cloned()
            .collect(),
        relations: catalog.relations[first_relation..]
            .iter()
            .take_while(|r| r.path == path)
            .cloned()
            .collect(),
        warnings: Vec::new(),
        truncated: catalog.truncated,
    }
}

fn v8_class_projection(graph_id: &str, grouped: &PublicationRows<'_>) -> (String, String) {
    let mut selected_classes = grouped.classes.clone();
    selected_classes.sort_by(|a, b| a.symbol.id.cmp(&b.symbol.id));
    let mut selected_relations = grouped.relations.clone();
    selected_relations.sort_by(|a, b| a.id.cmp(&b.id));
    let class_hash = v8_id(
        "",
        b"trellis.class-projection.v1\0",
        serde_json::json!({
            "graphProjectionId": graph_id,
            "classes": selected_classes, "relations": selected_relations,
        }),
    );
    let class_hash = class_hash.trim_start_matches(':').to_owned();
    let class_id = format!("class-projection:v1:{class_hash}");
    (class_id, class_hash)
}

fn v8_document_projection(
    file: &SourceFile,
    native: &crate::native_evidence::Artifact,
    grouped: &PublicationRows<'_>,
) -> Result<V8DocumentProjection> {
    let context = crate::native_evidence::native_extraction_context(
        &native.producer,
        &file.language,
        &native.revision,
    )?;
    let version_id = v8_id(
        "document:v1",
        b"trellis.document-version.v1\0",
        serde_json::json!({
            "sourceSetId": native.source_set.id, "language": file.language, "path": file.path,
            "contentHash": file.hash, "extractionContext": context,
            "producerId": native.producer.id, "producerVersion": native.producer.version,
        }),
    );
    let mut nodes = grouped.nodes.clone();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let mut calls = grouped.calls.clone();
    calls.sort_by(|a, b| a.id.cmp(&b.id));
    let mut regions = grouped.regions.clone();
    regions.sort_by(|a, b| a.id.cmp(&b.id));
    let graph_hash = v8_id(
        "",
        b"trellis.graph-projection.v1\0",
        serde_json::json!({
            "documentVersionId": version_id,
            "nodes": nodes, "calls": calls, "regions": regions,
        }),
    );
    let graph_hash = graph_hash.trim_start_matches(':').to_owned();
    let graph_id = format!("graph:v1:{graph_hash}");
    let (class_id, class_hash) = v8_class_projection(&graph_id, grouped);
    Ok(V8DocumentProjection {
        version_id,
        graph_id,
        class_id,
        graph_hash,
        class_hash,
    })
}

// Reuse decisions are made against one authenticated head before BEGIN IMMEDIATE.
// The writer checks the witnessed pin again under its CAS before using these IDs.
#[derive(Clone, Copy, Debug, Default)]
struct ReusedFamilies {
    native: bool,
    graph: bool,
    class: bool,
}

#[derive(Default)]
struct PreflightReuse {
    pin: Option<IndexPin>,
    families: BTreeMap<String, ReusedFamilies>,
    // The fast local route carries IDs only from the selected validated head.
    // No unchanged occurrence row is decoded or re-derived for publication.
    existing_ids: BTreeMap<String, V8DocumentProjection>,
}

pub(crate) struct LocalPrepared {
    pub(crate) changed_paths: BTreeSet<String>,
    removed_paths: BTreeSet<String>,
    pub(crate) native: crate::native_evidence::Artifact,
    prior_pin: IndexPin,
    prior_ids: BTreeMap<String, V8DocumentProjection>,
    prior_coverage: BTreeMap<String, crate::native_evidence::Coverage>,
    prior_summaries: BTreeMap<String, crate::classes::FileSummary>,
    prior_class_complete: BTreeMap<String, bool>,
    old_stats: IndexStats,
    old_diagnostics: Vec<Diagnostic>,
    old_changed_counts: (usize, usize, usize),
}
impl PreflightReuse {
    fn for_path(&self, path: &str) -> ReusedFamilies {
        self.families.get(path).copied().unwrap_or_default()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct RowByteCount {
    pub rows: u64,
    pub bytes: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WriterCounters {
    pub manifest: RowByteCount,
    pub native: RowByteCount,
    pub graph: RowByteCount,
    pub class: RowByteCount,
    pub total: RowByteCount,
    pub reused_occurrence_reads: u64,
}
impl WriterCounters {
    fn record(&mut self, family: &str, values: &[&dyn rusqlite::ToSql]) -> Result<()> {
        use rusqlite::types::{ToSqlOutput, Value, ValueRef};
        let mut bytes = 0u64;
        for value in values {
            bytes += match value.to_sql()? {
                ToSqlOutput::Borrowed(ValueRef::Null) | ToSqlOutput::Owned(Value::Null) => 0,
                ToSqlOutput::Borrowed(ValueRef::Integer(_) | ValueRef::Real(_))
                | ToSqlOutput::Owned(Value::Integer(_) | Value::Real(_)) => 8,
                ToSqlOutput::Borrowed(ValueRef::Text(v) | ValueRef::Blob(v)) => v.len() as u64,
                ToSqlOutput::Owned(Value::Text(v)) => v.len() as u64,
                ToSqlOutput::Owned(Value::Blob(v)) => v.len() as u64,
                _ => anyhow::bail!("unsupported SQLite writer counter value"),
            };
        }
        let group = match family {
            "manifest" => &mut self.manifest,
            "native" => &mut self.native,
            "graph" => &mut self.graph,
            "class" => &mut self.class,
            _ => anyhow::bail!("unknown writer counter family"),
        };
        group.rows += 1;
        group.bytes += bytes;
        self.total.rows += 1;
        self.total.bytes += bytes;
        Ok(())
    }
}

// New and historical version/projection IDs are canonical. A changed document
// can collide with retained history; those non-preflight collisions are compared
// before reuse rather than silently accepting corrupt or incomplete evidence.
#[derive(Default)]
struct ImmutableAppend {
    schema: BTreeMap<String, (Vec<String>, Vec<usize>)>,
    reused: BTreeMap<String, BTreeSet<String>>,
    expected: BTreeMap<(String, String), i64>,
    counters: WriterCounters,
}
impl ImmutableAppend {
    fn insert_manifest(
        &mut self,
        db: &Connection,
        sql: &str,
        values: &[&dyn rusqlite::ToSql],
    ) -> Result<()> {
        ensure!(
            db.execute(sql, values)? == 1,
            "incompatible_index: missing manifest row"
        );
        self.counters.record("manifest", values)
    }
    fn table(sql: &str) -> Result<&str> {
        let table = sql
            .split_whitespace()
            .nth(2)
            .context("missing immutable table")?
            .split('(')
            .next()
            .context("missing immutable table name")?;
        ensure!(
            matches!(
                table,
                "native_producers"
                    | "native_producer_languages"
                    | "native_source_sets"
                    | "native_source_set_languages"
                    | "native_source_set_dependencies"
                    | "document_versions"
                    | "graph_projections"
                    | "class_projections"
                    | "graph_nodes"
                    | "graph_display_labels"
                    | "classes"
                    | "class_relations"
                    | "native_version_coverage_roles"
                    | "native_version_search_cards"
                    | "native_version_declarations"
                    | "native_version_declaration_ancestors"
                    | "native_version_own_signature_types"
                    | "native_version_ancestor_signature_types"
                    | "native_version_headers"
                    | "native_version_header_items"
                    | "native_version_parameters"
                    | "native_version_references"
                    | "native_version_capability_families"
                    | "native_version_capability_gaps"
                    | "native_version_calls"
                    | "native_version_control_regions"
                    | "native_version_call_regions"
            ),
            "incompatible_index: unrecognized immutable table"
        );
        Ok(table)
    }
    fn key(db: &Connection, values: &[&dyn rusqlite::ToSql], i: usize) -> Result<String> {
        Ok(db
            .prepare_cached("SELECT CAST(?1 AS TEXT)")?
            .query_row([values[i]], |row| row.get(0))?)
    }
    fn group(
        table: &str,
        db: &Connection,
        values: &[&dyn rusqlite::ToSql],
    ) -> Result<Option<String>> {
        let key = match table {
            "native_producer_languages" => Some(format!(
                "{}\u{1f}{}",
                Self::key(db, values, 0)?,
                Self::key(db, values, 1)?
            )),
            "native_source_set_languages"
            | "native_source_set_dependencies"
            | "graph_nodes"
            | "graph_display_labels"
            | "classes"
            | "class_relations"
            | "native_version_coverage_roles"
            | "native_version_search_cards"
            | "native_version_declarations"
            | "native_version_declaration_ancestors"
            | "native_version_own_signature_types"
            | "native_version_ancestor_signature_types"
            | "native_version_headers"
            | "native_version_header_items"
            | "native_version_parameters"
            | "native_version_references"
            | "native_version_capability_families"
            | "native_version_capability_gaps"
            | "native_version_calls"
            | "native_version_control_regions"
            | "native_version_call_regions" => Some(Self::key(db, values, 0)?),
            _ => None,
        };
        Ok(key)
    }
    fn insert(
        &mut self,
        db: &Connection,
        sql: &str,
        values: &[&dyn rusqlite::ToSql],
    ) -> Result<usize> {
        let table = Self::table(sql)?;
        if !self.schema.contains_key(table) {
            let mut stmt = db.prepare(&format!("PRAGMA table_info({table})"))?;
            let mut columns = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            // Parent surrogates are SQLite rowids, not caller-supplied values.
            // Equality and dedupe still use the canonical natural key.
            if matches!(
                table,
                "document_versions" | "graph_projections" | "class_projections"
            ) {
                columns.remove(0);
            }
            let keys = if matches!(table, "graph_projections") {
                vec![2] // graph_hash
            } else if matches!(table, "class_projections") {
                vec![1] // content_hash
            } else if table == "document_versions" {
                vec![0] // id
            } else {
                columns
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, order))| *order > 0)
                    .map(|(i, _)| i)
                    .collect::<Vec<_>>()
            };
            ensure!(
                !keys.is_empty(),
                "incompatible_index: immutable table has no key"
            );
            let names = columns.drain(..).map(|(name, _)| name).collect::<Vec<_>>();
            self.schema.insert(table.to_owned(), (names, keys));
        }
        let (columns, keys) = &self.schema[table];
        ensure!(
            columns.len() == values.len(),
            "incompatible_index: immutable column count"
        );
        let predicate = |indices: &[usize]| {
            indices
                .iter()
                .map(|i| format!("{} IS ?{}", columns[*i], i + 1))
                .collect::<Vec<_>>()
                .join(" AND ")
        };
        let key_predicate = keys
            .iter()
            .enumerate()
            .map(|(parameter, i)| format!("{} IS ?{}", columns[*i], parameter + 1))
            .collect::<Vec<_>>()
            .join(" AND ");
        let key_values = keys.iter().map(|i| values[*i]).collect::<Vec<_>>();
        let exists: i64 = db
            .prepare_cached(&format!(
                "SELECT EXISTS(SELECT 1 FROM {table} WHERE {key_predicate})"
            ))?
            .query_row(key_values.as_slice(), |row| row.get(0))?;
        let inserted = if exists != 0 {
            if !matches!(
                table,
                "native_producers"
                    | "native_producer_languages"
                    | "native_source_sets"
                    | "native_source_set_languages"
                    | "native_source_set_dependencies"
            ) {
                self.counters.reused_occurrence_reads += 1;
            }
            let all = (0..columns.len()).collect::<Vec<_>>();
            let exact: i64 = db
                .prepare_cached(&format!(
                    "SELECT EXISTS(SELECT 1 FROM {table} WHERE {})",
                    predicate(&all)
                ))?
                .query_row(values, |row| row.get(0))?;
            control_ensure!(
                exact == 1,
                format!("incompatible_index: immutable {table} differs from captured evidence")
            );
            0
        } else {
            db.prepare_cached(sql)?.execute(values)?
        };
        if inserted != 0 {
            let family = if table.starts_with("native_") || table == "document_versions" {
                "native"
            } else if table.starts_with("graph_") {
                "graph"
            } else {
                "class"
            };
            self.counters.record(family, values)?;
        }
        let parent = match table {
            "native_producers" => Some(format!(
                "{}\u{1f}{}",
                Self::key(db, values, 0)?,
                Self::key(db, values, 1)?
            )),
            "native_source_sets" => Some(Self::key(db, values, 0)?),
            "document_versions" | "graph_projections" | "class_projections" => {
                let (key, column, index) = match table {
                    "document_versions" => ("vid", "id", 0),
                    "graph_projections" => ("gid", "graph_hash", 2),
                    _ => ("cid", "content_hash", 1),
                };
                Some(
                    db.query_row(
                        &format!("SELECT {key} FROM {table} WHERE {column}=?1"),
                        [values[index]],
                        |row| row.get::<_, i64>(0),
                    )?
                    .to_string(),
                )
            }
            _ => None,
        };
        if exists != 0
            && let Some(parent) = parent
        {
            self.reused
                .entry(table.to_owned())
                .or_default()
                .insert(parent);
        }
        if let Some(parent) = Self::group(table, db, values)? {
            *self.expected.entry((table.to_owned(), parent)).or_default() += 1;
        }
        Ok(inserted)
    }
    fn finish(&self, db: &Connection) -> Result<()> {
        const FAMILIES: &[(&str, &[(&str, &str)])] = &[
            (
                "native_producers",
                &[
                    ("native_producer_languages", "producer"),
                    ("native_producer_inputs", "producer"),
                ],
            ),
            (
                "native_source_sets",
                &[
                    ("native_source_set_languages", "source_set_id"),
                    ("native_source_set_dependencies", "source_set_id"),
                ],
            ),
            (
                "document_versions",
                &[
                    ("native_version_coverage_roles", "vid"),
                    ("native_version_search_cards", "vid"),
                    ("native_version_declarations", "vid"),
                    ("native_version_declaration_ancestors", "vid"),
                    ("native_version_own_signature_types", "vid"),
                    ("native_version_ancestor_signature_types", "vid"),
                    ("native_version_headers", "vid"),
                    ("native_version_header_items", "vid"),
                    ("native_version_parameters", "vid"),
                    ("native_version_references", "vid"),
                    ("native_version_capability_families", "vid"),
                    ("native_version_capability_gaps", "vid"),
                    ("native_version_calls", "vid"),
                    ("native_version_control_regions", "vid"),
                    ("native_version_call_regions", "vid"),
                ],
            ),
            (
                "graph_projections",
                &[("graph_nodes", "gid"), ("graph_display_labels", "gid")],
            ),
            (
                "class_projections",
                &[("classes", "cid"), ("class_relations", "cid")],
            ),
        ];
        for (parent_table, children) in FAMILIES {
            if let Some(reused) = self.reused.get(*parent_table) {
                for parent in reused {
                    for (child, column) in *children {
                        let actual: i64 = if *column == "producer" {
                            let (id, version) = parent
                                .split_once('\u{1f}')
                                .context("invalid producer key")?;
                            db.query_row(
                                &format!("SELECT count(*) FROM {child} WHERE producer_id=?1 AND producer_version=?2"),
                                params![id, version], |row| row.get(0),
                            )?
                        } else {
                            db.query_row(
                                &format!("SELECT count(*) FROM {child} WHERE {column}=?1"),
                                [parent],
                                |row| row.get(0),
                            )?
                        };
                        let expected = self
                            .expected
                            .get(&(child.to_string(), parent.clone()))
                            .copied()
                            .unwrap_or(0);
                        control_ensure!(
                            actual == expected,
                            format!(
                                "incompatible_index: incomplete immutable {child} for {parent_table}"
                            )
                        );
                    }
                }
            }
        }
        Ok(())
    }
}

fn graph_id_row(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<String> {
    row.get::<_, String>(index)
        .map(|hash| format!("graph:v1:{hash}"))
}
fn class_id_row(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<String>> {
    row.get::<_, Option<String>>(index)
        .map(|hash| hash.map(|hash| format!("class-projection:v1:{hash}")))
}

fn stored_sid(id: &str) -> Result<Vec<u8>> {
    id_codec::encode_sid(id).context("native_evidence_required: invalid syntax ID")
}
fn stored_occ(id: &str) -> Result<Vec<u8>> {
    id_codec::encode_occ(id).context("native_evidence_required: invalid occurrence ID")
}

// Emit exactly one document's graph and class rows under the caller's transaction.
fn write_graph_document(
    db: &Connection,
    grouped: &PublicationRows<'_>,
    ids: &V8DocumentProjection,
    reused: ReusedFamilies,
    immutable: &mut ImmutableAppend,
) -> Result<()> {
    let gid: i64 = db
        .prepare_cached("SELECT gid FROM graph_projections WHERE graph_hash=?1")?
        .query_row([&ids.graph_hash], |row| row.get(0))?;
    let cid: i64 = db
        .prepare_cached("SELECT cid FROM class_projections WHERE content_hash=?1")?
        .query_row([&ids.class_hash], |row| row.get(0))?;
    for n in &grouped.nodes {
        if reused.graph {
            continue;
        }
        let kind = serde_json::to_value(n.kind)?;
        let kind = kind
            .as_str()
            .context("incompatible_index: selected graph kind invalid")?;
        immutable.insert(
            db,
            "INSERT INTO graph_nodes VALUES(?1,?2,?3,?4)",
            params![gid, stored_sid(&n.id)?, n.name, kind],
        )?;
    }
    for class in &grouped.classes {
        if reused.class {
            continue;
        }
        immutable.insert(
            db,
            "INSERT INTO classes VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![
                cid,
                gid,
                stored_sid(&class.symbol.id)?,
                class.symbol.name,
                class.qualified_name,
                class.symbol.path,
                json(class)?,
            ],
        )?;
    }
    for relation in &grouped.relations {
        if reused.class {
            continue;
        }
        immutable.insert(
            db,
            "INSERT INTO class_relations VALUES(?1,?2,?3,?4,?5)",
            params![
                cid,
                relation.id,
                stored_sid(&relation.owner)?,
                relation.target.as_deref().map(stored_sid).transpose()?,
                json(relation)?,
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static WRITER_SOURCE_SQL_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LOCAL_CLASS_EXTRACTION_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static LOCAL_CLASS_PROJECTION_HASHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
fn writer_source_sql_reads_for_tests() -> usize {
    WRITER_SOURCE_SQL_READS.with(std::cell::Cell::get)
}

/// A document's projection IDs and native witness, computed off the SQL
/// thread. `None` means the reuse path supplies it, or it is recomputed inline.
type PrecomputedDocument = (Option<Result<V8DocumentProjection>>, Option<Result<String>>);

/// One document's projection and witness, under write_native's reuse conditions.
fn precompute_document(
    file: &SourceFile,
    native: &crate::native_evidence::Artifact,
    grouped: Option<&PublicationRows<'_>>,
    document: Option<&crate::native_evidence::Document>,
    coverage: Option<&crate::native_evidence::Coverage>,
    reuse: &PreflightReuse,
) -> PrecomputedDocument {
    let projection = if reuse.existing_ids.contains_key(&file.path) {
        None
    } else {
        grouped.map(|grouped| v8_document_projection(file, native, grouped))
    };
    let witness =
        if reuse.for_path(&file.path).native {
            None
        } else {
            match (document, coverage, grouped) {
                (Some(document), Some(coverage), Some(grouped)) => Some(
                    publication_native_witness(document, &native.producer, coverage, grouped),
                ),
                _ => None,
            }
        };
    (projection, witness)
}

// The full-artifact caller writes its header and all documents together.
// The streamed caller writes its header once, then one checked document at a
// time; these distinct modes cannot be mixed by positional flags.
enum NativeWrite<'a> {
    Header {
        ordinal_start: usize,
    },
    Document {
        ordinal: usize,
        captured_file: &'a SourceFile,
        precomputed: PrecomputedDocument,
    },
}

fn semantic_variant<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_value(value)?
        .as_str()
        .context("invalid semantic variant")?
        .to_owned())
}

fn semantic_target_values(
    target: &crate::semantic_evidence::Target,
) -> Result<Vec<rusqlite::types::Value>> {
    use crate::semantic_evidence::Target;
    use rusqlite::types::Value;
    Ok(match target {
        Target::Internal {
            syntax_id,
            document,
            revision_id,
        } => vec![
            Value::Text("internal".into()),
            Value::Blob(
                id_codec::encode_sid(syntax_id)
                    .context("invalid semantic syntax ID")?
                    .to_vec(),
            ),
            Value::Text(document.language.clone()),
            Value::Text(document.path.clone()),
            Value::Text(revision_id.clone()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
        ],
        Target::External { symbol } => vec![
            Value::Text("external".into()),
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Null,
            Value::Text(semantic_variant(&symbol.scheme)?),
            Value::Text(symbol.symbol.clone()),
            Value::Text(semantic_variant(&symbol.scope)?),
            symbol
                .document
                .as_ref()
                .map(|d| Value::Text(d.language.clone()))
                .unwrap_or(Value::Null),
            symbol
                .document
                .as_ref()
                .map(|d| Value::Text(d.path.clone()))
                .unwrap_or(Value::Null),
        ],
    })
}

/// `Connection::execute` through the statement cache: the semantic writers run the same few
/// statements once per row of every publication.
fn execute_cached(
    db: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> rusqlite::Result<usize> {
    db.prepare_cached(sql)?.execute(params)
}

/// `Connection::query_row` through the statement cache.
fn query_row_cached<T>(
    db: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
    map: impl FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<T> {
    db.prepare_cached(sql)?.query_row(params, map)
}

fn insert_semantic_target(
    db: &Connection,
    table: &str,
    parent: &[rusqlite::types::Value],
    target: &crate::semantic_evidence::Target,
) -> Result<()> {
    let mut values = parent.to_vec();
    values.extend(semantic_target_values(target)?);
    let placeholders = (1..=values.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(",");
    execute_cached(
        db,
        &format!("INSERT INTO {table} VALUES({placeholders})"),
        rusqlite::params_from_iter(values),
    )?;
    Ok(())
}

/// Metadata authenticated at admission; no decoded claim payload is available in LOCAL reuse.
#[derive(Debug, Clone)]
pub(crate) struct WitnessedRawCapture {
    pub producer_id: String,
    pub source_set_id: String,
    pub captured_revision: String,
    pub binary_bytes: Vec<u8>,
    pub inventory: crate::scip_raw::ProofInventory,
}

pub(crate) struct AttestedProfileProducer {
    id: String,
    version: String,
    executable_hash: String,
    position_encoding: crate::semantic_evidence::PositionEncoding,
    languages: Vec<crate::semantic_evidence::Language>,
}

impl AttestedProfileProducer {
    /// The profile pin of `producer`, for tests that run the whole-publication validator over a
    /// publication without a stored raw batch.
    #[cfg(feature = "test-support")]
    pub(crate) fn pinned_for_tests(producer: &crate::semantic_evidence::Producer) -> Self {
        Self {
            id: producer.id.clone(),
            version: producer.version.clone(),
            executable_hash: producer.executable_hash.clone(),
            position_encoding: producer.position_encoding,
            languages: producer.languages.clone(),
        }
    }

    pub(crate) fn matches(&self, producer: &crate::semantic_evidence::Producer) -> bool {
        producer.id == self.id
            && producer.version == self.version
            && producer.executable_hash == self.executable_hash
            && producer.position_encoding == self.position_encoding
            && producer.kind == crate::semantic_evidence::SemanticKind::Semantic
            && producer.languages == self.languages
    }
}

/// Every original artifact of a stored batch, keyed for `CapturedComponents`.
fn captured_artifacts(raw: &SelectedRawBatch) -> BTreeMap<(String, String), Vec<u8>> {
    raw.captures
        .iter()
        .map(|capture| {
            (
                (raw.producer_id.clone(), capture.artifact_hash.clone()),
                capture.binary_bytes.clone(),
            )
        })
        .collect()
}

/// Re-authenticates a stored batch against its retained captured revision: the pinned
/// producer and header binding, the source manifest, then a streaming decode of every
/// original artifact, whose claims must equal the stored proof inventory.
fn authenticate_stored_batch(
    raw: &SelectedRawBatch,
    inventory: &crate::scip_raw::CapturedScipInventory,
    captured: &crate::semantic_evidence::CapturedComponents,
    r1: &crate::native_evidence::Artifact,
    sources: &BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    root: &std::path::Path,
    cancel: &CancelFlag,
) -> Result<AttestedProfileProducer> {
    use crate::native_ids::digest;
    use crate::scip_raw::{
        self, AuthenticatedArtifact, AuthenticatedScipDocument, DocumentSource, VecSink,
    };
    use crate::semantic_batch::{
        BatchCounters, BatchLimits,
        descriptor::{FORMAT_VERSION, SourceManifestHasher},
    };
    let descriptor = &raw.descriptor;
    ensure!(
        descriptor.format_version == FORMAT_VERSION
            && descriptor.decoder_version == scip_raw::DECODER_VERSION
            && raw.decoder_version == scip_raw::DECODER_VERSION,
        "raw batch descriptor version"
    );
    let pinned = descriptor
        .check_producer()
        .context("raw producer not pinned")?;
    descriptor
        .bind_header(root, &r1.source_set, &r1.revision)
        .context("raw batch header binding")?;
    let mut manifest_hash = SourceManifestHasher::new(&r1.source_set.id);
    for doc in &r1.revision.documents {
        manifest_hash.push(&doc.key.language, &doc.key.path, &doc.content_hash);
    }
    ensure!(
        descriptor.source_manifest_hash == raw.source_manifest_hash
            && descriptor.source_manifest_hash == manifest_hash.finish()
            && descriptor.totals.source_documents == r1.revision.documents.len() as u64,
        "raw batch does not bind captured revision source manifest"
    );
    ensure!(
        raw.producer_id == pinned.name
            && raw.source_set_id == r1.source_set.id
            && raw.captured_revision == r1.revision.id
            && captured.captured_revision == r1.revision
            && captured.sources == *sources,
        "raw captured native/source identity differs"
    );
    let uri = crate::scip_admission::file_uri(root).context("raw project root")?;
    let module_roots = scip_raw::ModuleRoots::from_native_config(&captured.native_config);
    // Cargo targets read from every captured Rust document, as admission read them (#394).
    let mut cargo_targets =
        scip_raw::CargoTargetsBuilder::from_native_config(&captured.native_config);
    if !cargo_targets.is_empty() && pinned.languages.iter().any(|l| l == "rust") {
        for doc in r1
            .revision
            .documents
            .iter()
            .filter(|d| d.key.language == "rust")
        {
            let bytes = sources
                .get(&doc.key)
                .context("raw retained document missing")?;
            cargo_targets.add_document(&doc.key.path, bytes);
        }
    }
    let cargo_targets = cargo_targets.finish();
    // The limits admission ran under, so a document capped there is capped here.
    let limits = BatchLimits::effective().map_err(anyhow::Error::msg)?;
    let mut claims = Vec::new();
    let mut capped = scip_raw::CappedDocuments::new();
    for capture in &raw.captures {
        // The artifact covers every retained document of its language that the batch did not
        // exclude, unless a stored document row assigns it to another artifact.
        let excluded: BTreeSet<_> = raw
            .exclusions
            .iter()
            .map(|exclusion| {
                (
                    exclusion.language.as_str(),
                    exclusion.relative_path.as_str(),
                )
            })
            .collect();
        let mut docs = Vec::new();
        for doc in &r1.revision.documents {
            let (language, path) = (&doc.key.language, &doc.key.path);
            if *language != capture.language
                || excluded.contains(&(language.as_str(), path.as_str()))
                || raw
                    .documents
                    .get(&(language.clone(), path.clone()))
                    .is_some_and(|(_, _, capture_id)| *capture_id != capture.capture_id)
            {
                continue;
            }
            let bytes = sources
                .get(&doc.key)
                .context("raw retained document missing")?;
            #[cfg(feature = "test-support")]
            crate::semantic_import::note_original_source_hash(&doc.key);
            ensure!(
                digest(b"", bytes) == doc.content_hash
                    && raw
                        .documents
                        .get(&(language.clone(), path.clone()))
                        .is_none_or(|(source_hash, _, _)| *source_hash == doc.content_hash),
                "raw retained document differs"
            );
            docs.push(AuthenticatedScipDocument {
                language: language.clone(),
                relative_path: path.clone(),
                source_bytes: bytes,
                content_hash: doc.content_hash.clone(),
            });
        }
        #[cfg(feature = "test-support")]
        crate::semantic_import::note_original_raw_decode();
        ensure!(
            scip_raw::sha(&capture.binary_bytes) == capture.artifact_hash,
            "raw original binary admission: artifact digest"
        );
        let artifact = AuthenticatedArtifact {
            artifact_hash: &capture.artifact_hash,
            producer_id: &raw.producer_id,
            producer_version: &pinned.version,
            source_set_id: &raw.source_set_id,
            source_revision: &raw.captured_revision,
            position_encoding: pinned.position_encoding,
            protocol_version: 0,
            text_document_encoding: 1,
            project_root_uri: &uri,
            module_roots: &module_roots,
            cargo_targets: &cargo_targets,
            assigned_documents: docs.len() as u64,
        };
        let by_path: BTreeMap<&str, &AuthenticatedScipDocument<'_>> = docs
            .iter()
            .map(|doc| (doc.relative_path.as_str(), doc))
            .collect();
        ensure!(
            by_path.len() == docs.len(),
            "raw original binary admission: duplicate source document"
        );
        let mut resolve = |path: &str, language: &str| {
            let source = by_path.get(path).ok_or_else(|| {
                scip_raw::ScipCaptureError::InvalidArtifact("unlisted document".into())
            })?;
            let language =
                scip_raw::bind_document_language(language, &source.language, &pinned.languages)?;
            Ok(DocumentSource {
                language: language.to_owned(),
                bytes: std::borrow::Cow::Borrowed(source.source_bytes),
                content_hash: source.content_hash.clone(),
            })
        };
        let mut sink = VecSink::default();
        scip_raw::decode_streaming(
            capture.binary_bytes.as_slice(),
            capture.binary_bytes.len() as u64,
            &artifact,
            &mut resolve,
            &mut BatchCounters::default(),
            &limits,
            &mut sink,
            cancel,
        )
        .map_err(anyhow::Error::from)
        .context("raw original binary admission")?;
        capped.extend(sink.capped_documents().iter().cloned());
        claims.extend(sink.finish().context("raw original binary admission")?);
    }
    claims.sort_by(|a, b| a.captured_proof_id.cmp(&b.captured_proof_id));
    let mut retained = inventory.claims.clone();
    retained.sort_by(|a, b| a.captured_proof_id.cmp(&b.captured_proof_id));
    ensure!(
        inventory.artifact_hashes
            == raw
                .captures
                .iter()
                .map(|capture| capture.artifact_hash.clone())
                .collect::<Vec<_>>()
            && inventory.decoder_version == scip_raw::DECODER_VERSION
            && inventory.source_manifest_hash == raw.source_manifest_hash
            && inventory.completion == descriptor.completion
            && inventory.exclusions == raw.exclusions
            && inventory.capped_documents == capped
            && claims == retained,
        "raw inventory differs from authenticated binary"
    );
    let attested = pinned.producer();
    Ok(AttestedProfileProducer {
        id: raw.producer_id.clone(),
        version: attested.version,
        executable_hash: attested.executable_hash,
        position_encoding: attested.position_encoding,
        languages: attested.languages,
    })
}

#[cfg(test)]
mod selected_raw_batch_tests;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredRawProof {
    pub record_kind: String,
    pub fact_digest: String,
    pub language: String,
    pub path: String,
    pub canonical_bytes: Vec<u8>,
    pub artifact_hash: String,
}

/// One original artifact of a selected batch.
#[derive(Debug, Clone)]
pub struct SelectedRawCapture {
    pub capture_id: i64,
    pub ordinal: u32,
    pub language: String,
    pub artifact_hash: String,
    pub binary_bytes: Vec<u8>,
}

/// One accepted raw batch selected by a publication pin. The proof inventory is
/// the original capture, not the subset that contributed selected provenance.
#[derive(Debug, Clone)]
pub struct SelectedRawBatch {
    pub producer: crate::semantic_evidence::Producer,
    pub requested_documents: Vec<crate::native_evidence::DocumentKey>,
    pub producer_id: String,
    pub source_set_id: String,
    pub captured_revision: String,
    pub batch_id: i64,
    pub descriptor: crate::semantic_batch::descriptor::Descriptor,
    pub source_manifest_hash: String,
    pub decoder_version: String,
    /// In ordinal order.
    pub captures: Vec<SelectedRawCapture>,
    pub exclusions: Vec<crate::scip_raw::Exclusion>,
    /// Documents admitted under `scip_raw::OCCURRENCE_CAP_EXCEEDED`.
    pub capped_documents: crate::scip_raw::CappedDocuments,
    pub proofs: Vec<(String, StoredRawProof)>,
    /// `(language, path)` to `(source_hash, wire_digest, capture_id)`.
    pub documents: BTreeMap<(String, String), (String, String, i64)>,
    pub captured_pin: IndexPin,
}

impl SelectedRawBatch {
    /// The artifact that carries a document's basis (§4.4 e): its assigned artifact when
    /// covered, otherwise the lowest-ordinal artifact of its language, otherwise artifact 0.
    pub fn artifact_hash_for(&self, doc: &crate::native_evidence::DocumentKey) -> &str {
        let assigned = self
            .documents
            .get(&(doc.language.clone(), doc.path.clone()))
            .and_then(|(_, _, capture_id)| {
                self.captures
                    .iter()
                    .find(|capture| capture.capture_id == *capture_id)
            })
            .map(|capture| capture.ordinal);
        let ordinal = crate::semantic_batch::descriptor::basis_artifact_ordinal(
            &self.descriptor.artifacts,
            &doc.language,
            assigned,
        );
        self.captures
            .iter()
            .find(|capture| capture.ordinal == ordinal)
            .or_else(|| self.captures.first())
            .map_or("", |capture| capture.artifact_hash.as_str())
    }
}

type CapturedNativeInput = (
    crate::native_evidence::Artifact,
    BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    (
        serde_json::Value,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    ),
);
type SelectedRawBatchResult = (
    crate::semantic_evidence::Producer,
    Vec<crate::native_evidence::DocumentKey>,
    Result<SelectedRawBatch>,
);

struct SelectedNativeInput {
    lightweight: bool,
    proof_identities: Vec<crate::scip_raw::RawProofIdentity>,
    producer: crate::semantic_evidence::Producer,
    requested_documents: Vec<crate::native_evidence::DocumentKey>,
    raw: Result<SelectedRawBatch>,
    captured: Result<CapturedNativeInput>,
}

fn semantic_pin_key(pin: IndexPin) -> String {
    format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision)
}

type SemanticCoverageRow = (
    i64,
    String,
    String,
    String,
    String,
    bool,
    bool,
    String,
    Option<String>,
);

fn copy_semantic_rows(db: &Connection, prior: &str, next: &str, exact: bool) -> Result<()> {
    copy_semantic_rows_filtered(db, prior, next, exact, None, &[])
}
fn copy_semantic_rows_except(
    db: &Connection,
    prior: &str,
    next: &str,
    producer: &str,
) -> Result<()> {
    copy_semantic_rows_filtered(db, prior, next, true, Some(producer), &[])
}
fn copy_semantic_rows_filtered(
    db: &Connection,
    prior: &str,
    next: &str,
    exact: bool,
    except: Option<&str>,
    excluded_inputs: &[SelectedNativeInput],
) -> Result<()> {
    let mut statement = db.prepare("SELECT tuple_id,producer_id,producer_version,language,path,requested,selected,state,diagnostic FROM semantic_coverage WHERE revision_id=?1 ORDER BY tuple_id")?;
    let tuples: Vec<SemanticCoverageRow> = statement
        .query_map([prior], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    for (old, producer, version, language, path, requested, selected, state, diagnostic) in tuples {
        if except == Some(producer.as_str())
            || excluded_inputs
                .iter()
                .any(|input| input.producer.id == producer)
        {
            continue;
        }
        let applicable: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?1 AND language=?2 AND path=?3)",params![next,language,path],|row|row.get(0))?;
        if !applicable {
            continue;
        }
        db.execute("INSERT INTO semantic_coverage(revision_id,producer_id,producer_version,language,path,requested,selected,state,diagnostic) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![next,producer,version,language,path,requested,if exact { selected } else { false },if exact {state.as_str()} else if requested {"omitted"} else {"notRequested"},if exact {diagnostic.as_deref()} else if requested {Some("semantic producer not run for this revision")} else {None}])?;
        let tuple = db.last_insert_rowid();
        copy_semantic_table(db, "semantic_coverage_roles", "tuple_id", old, tuple)?;
        if !exact {
            db.execute(
                "DELETE FROM semantic_coverage_roles WHERE tuple_id=?1 AND kind='observed'",
                [tuple],
            )?;
            continue;
        }
        let mut basis_ids = BTreeMap::new();
        for basis in semantic_table_ids(db, "semantic_basis", "tuple_id", old)? {
            let new = copy_semantic_surrogate(
                db,
                "semantic_basis",
                "basis_id",
                "tuple_id",
                basis,
                tuple,
            )?;
            basis_ids.insert(basis, new);
            copy_semantic_table(
                db,
                "semantic_basis_lookup_dependencies",
                "basis_id",
                basis,
                new,
            )?;
        }
        let mut proofs =
            db.prepare("SELECT provenance_id,basis_id FROM semantic_provenance WHERE tuple_id=?1 ORDER BY rowid")?;
        let proofs = proofs
            .query_map([old], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (proof, basis) in proofs {
            let new_basis = basis_ids
                .get(&basis)
                .context("missing copied semantic basis")?;
            db.execute("INSERT INTO semantic_provenance SELECT ?1,provenance_id,content_hash,evidence_kind,freshness,?2,capture_id,derived_from_provenance_id,derived_from_record_kind,derived_from_fact_digest FROM semantic_provenance WHERE tuple_id=?3 AND provenance_id=?4",params![tuple,new_basis,old,proof])?;
        }
        for (table, id, children) in [
            (
                "semantic_symbols",
                "symbol_id",
                &["semantic_symbol_targets"][..],
            ),
            (
                "semantic_declaration_bindings",
                "binding_id",
                &[
                    "semantic_declaration_binding_symbols",
                    "semantic_declaration_join_candidates",
                ][..],
            ),
            (
                "semantic_call_bindings",
                "binding_id",
                &[
                    "semantic_call_candidates",
                    "semantic_call_possible_dispatch",
                    "semantic_call_join_candidates",
                ][..],
            ),
            ("semantic_type_relationships", "relationship_id", &[][..]),
        ] {
            for old_id in semantic_table_ids(db, table, "tuple_id", old)? {
                let new_id = copy_semantic_surrogate(db, table, id, "tuple_id", old_id, tuple)?;
                for child in children {
                    copy_semantic_table(db, child, id, old_id, new_id)?;
                }
            }
        }
        let refs = semantic_table_blobs(db, "semantic_references", "reference_id", old)?;
        for id in refs {
            db.execute("INSERT INTO semantic_references SELECT ?1,reference_id,ordinal,owner_id,start_byte,end_byte,spelling,lookup_key,site,resolution,declared_kind,declared_internal_syntax_id,declared_internal_language,declared_internal_path,declared_internal_native_revision_id,declared_external_scheme,declared_external_symbol,declared_external_scope,declared_external_language,declared_external_path,provenance_id FROM semantic_references WHERE tuple_id=?2 AND reference_id=?3",params![tuple,old,id])?;
            for child in ["semantic_reference_roles", "semantic_reference_candidates"] {
                let columns = semantic_columns(db, child)?;
                let fields = columns
                    .iter()
                    .map(|c| {
                        if c == "tuple_id" {
                            "?1".to_string()
                        } else {
                            c.clone()
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                db.execute(&format!("INSERT INTO {child} SELECT {fields} FROM {child} WHERE tuple_id=?2 AND reference_id=?3"),params![tuple,old,id])?;
            }
        }
    }
    if except.is_none() && excluded_inputs.is_empty() {
        fill_semantic_coverage(db, prior, next)?;
    }
    Ok(())
}

fn fill_semantic_coverage(db: &Connection, prior: &str, next: &str) -> Result<()> {
    let missing = db
        .prepare(
            "SELECT p.id,p.version,d.language,d.path,
        EXISTS(SELECT 1 FROM semantic_coverage old WHERE old.revision_id=?1
            AND old.producer_id=p.id AND old.language=d.language
            AND old.path=d.path AND old.requested=1)
        FROM semantic_revision_producers p
        JOIN semantic_producer_languages l ON l.id=p.id AND l.version=p.version
        JOIN revision_documents d ON d.revision_id=?2 AND d.language=l.language
        WHERE p.revision_id=?2 AND NOT EXISTS(SELECT 1 FROM semantic_coverage current
            WHERE current.revision_id=?2 AND current.producer_id=p.id
            AND current.language=d.language AND current.path=d.path)
        ORDER BY p.id,d.language,d.path",
        )?
        .query_map(params![prior, next], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, bool>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (producer, version, language, path, requested) in missing {
        db.execute("INSERT INTO semantic_coverage(revision_id,producer_id,producer_version,language,path,requested,selected,state,diagnostic) VALUES(?1,?2,?3,?4,?5,?6,0,?7,?8)",
            params![next,producer,version,language,path,requested,if requested {"omitted"} else {"notRequested"},
                if requested {Some("semantic producer not run for this revision")} else {None}])?;
    }
    Ok(())
}
fn semantic_columns(db: &Connection, table: &str) -> Result<Vec<String>> {
    Ok(db
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?)
}
fn semantic_table_ids(db: &Connection, table: &str, column: &str, id: i64) -> Result<Vec<i64>> {
    Ok(db
        .prepare(&format!(
            "SELECT {} FROM {table} WHERE {column}=?1 ORDER BY {}",
            if column == "tuple_id" {
                if table == "semantic_basis" {
                    "basis_id"
                } else if table == "semantic_symbols" {
                    "symbol_id"
                } else if table == "semantic_declaration_bindings"
                    || table == "semantic_call_bindings"
                {
                    "binding_id"
                } else {
                    "relationship_id"
                }
            } else {
                column
            },
            if column == "tuple_id" {
                "rowid"
            } else {
                column
            }
        ))?
        .query_map([id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
fn semantic_table_blobs(
    db: &Connection,
    table: &str,
    column: &str,
    id: i64,
) -> Result<Vec<Vec<u8>>> {
    Ok(db
        .prepare(&format!(
            "SELECT {column} FROM {table} WHERE tuple_id=?1 ORDER BY rowid"
        ))?
        .query_map([id], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}
fn copy_semantic_table(
    db: &Connection,
    table: &str,
    column: &str,
    old: i64,
    new: i64,
) -> Result<()> {
    let columns = semantic_columns(db, table)?;
    let fields = columns
        .iter()
        .map(|c| {
            if c == column {
                "?1".to_string()
            } else {
                c.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    db.execute(
        &format!("INSERT INTO {table} SELECT {fields} FROM {table} WHERE {column}=?2"),
        params![new, old],
    )?;
    Ok(())
}
fn copy_semantic_surrogate(
    db: &Connection,
    table: &str,
    id: &str,
    parent: &str,
    old: i64,
    new: i64,
) -> Result<i64> {
    let columns = semantic_columns(db, table)?;
    let fields = columns
        .iter()
        .filter(|c| c.as_str() != id)
        .map(|c| {
            if c == parent {
                "?1".to_string()
            } else {
                c.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(",");
    let count = columns.len() - 1;
    let names = columns
        .into_iter()
        .filter(|c| c != id)
        .collect::<Vec<_>>()
        .join(",");
    ensure!(count > 0, "invalid semantic row");
    db.execute(
        &format!("INSERT INTO {table}({names}) SELECT {fields} FROM {table} WHERE {id}=?2"),
        params![new, old],
    )?;
    Ok(db.last_insert_rowid())
}

fn semantic_enum<T: serde::de::DeserializeOwned>(value: String) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(value)).map_err(|error| {
        SelectedIntegrity(format!(
            "incompatible_index: invalid semantic enum: {error}"
        ))
        .into()
    })
}
fn semantic_target_read(
    row: &rusqlite::Row<'_>,
    start: usize,
    source_set: &str,
) -> Result<Option<crate::semantic_evidence::Target>> {
    use crate::semantic_evidence as sem;
    let kind: Option<String> = row.get(start)?;
    Ok(match kind.as_deref() {
        None => {
            for column in start + 1..=start + 9 {
                selected_ensure!(
                    row.get::<_, Option<rusqlite::types::Value>>(column)?
                        .is_none(),
                    "incompatible_index: corrupt semantic target payload"
                );
            }
            None
        }
        Some("internal") => {
            let id: Vec<u8> = row.get(start + 1)?;
            Some(sem::Target::Internal {
                syntax_id: id_codec::decode_sid(&id).context(SelectedIntegrity(
                    "incompatible_index: corrupt semantic internal ID".into(),
                ))?,
                document: crate::native_evidence::DocumentKey {
                    source_set_id: source_set.into(),
                    language: row.get(start + 2)?,
                    path: row.get(start + 3)?,
                },
                revision_id: row.get(start + 4)?,
            })
        }
        Some("external") => {
            let language: Option<String> = row.get(start + 8)?;
            let path: Option<String> = row.get(start + 9)?;
            Some(sem::Target::External {
                symbol: sem::SymbolKey {
                    scheme: semantic_enum(row.get(start + 5)?)?,
                    symbol: row.get(start + 6)?,
                    scope: semantic_enum(row.get(start + 7)?)?,
                    document: match (language, path) {
                        (Some(language), Some(path)) => Some(crate::native_evidence::DocumentKey {
                            source_set_id: source_set.into(),
                            language,
                            path,
                        }),
                        (None, None) => None,
                        _ => {
                            return Err(SelectedIntegrity(
                                "incompatible_index: corrupt semantic target document".into(),
                            )
                            .into());
                        }
                    },
                },
            })
        }
        _ => {
            return Err(SelectedIntegrity(
                "incompatible_index: corrupt semantic target kind".into(),
            )
            .into());
        }
    })
}
fn semantic_targets(
    db: &Connection,
    table: &str,
    where_clause: &str,
    values: &[&dyn rusqlite::ToSql],
    source_set: &str,
) -> Result<Vec<crate::semantic_evidence::Target>> {
    let mut stmt = db.prepare(&format!(
        "SELECT * FROM {table} WHERE {where_clause} ORDER BY ordinal"
    ))?;
    let rows = stmt
        .query_map(values, |row| {
            // Child target rows carry parent ID then ordinal; reference child has tuple,ref,ordinal.
            let start = if table == "semantic_reference_candidates" {
                3
            } else {
                2
            };
            let target = semantic_target_read(row, start, source_set).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    start,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(e.to_string())),
                )
            })?;
            Ok((row.get::<_, i64>(start - 1)?, target))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut targets = Vec::new();
    for (ordinal, target) in rows {
        selected_ensure!(
            ordinal == targets.len() as i64,
            "incompatible_index: corrupt semantic target ordinal"
        );
        targets.push(target.context(SelectedIntegrity(
            "incompatible_index: corrupt null semantic target".into(),
        ))?);
    }
    Ok(targets)
}
fn semantic_ordered<T>(
    db: &Connection,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
) -> Result<Vec<T>>
where
    T: serde::de::DeserializeOwned,
{
    let raw = db
        .prepare(sql)?
        .query_map(values, |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut output = Vec::new();
    for (ordinal, value) in raw {
        selected_ensure!(
            ordinal == output.len() as i64,
            "incompatible_index: corrupt semantic child ordinal"
        );
        output.push(semantic_enum(value)?)
    }
    Ok(output)
}
fn semantic_symbol_key_read(
    scheme: String,
    symbol: String,
    scope: String,
    language: Option<String>,
    path: Option<String>,
    source_set: &str,
) -> Result<crate::semantic_evidence::SymbolKey> {
    use crate::semantic_evidence as sem;
    let scope = semantic_enum(scope)?;
    let document = match (scope, language, path) {
        (sem::SymbolScope::Global, None, None) => None,
        (sem::SymbolScope::Document | sem::SymbolScope::Target, Some(language), Some(path)) => {
            Some(crate::native_evidence::DocumentKey {
                source_set_id: source_set.into(),
                language,
                path,
            })
        }
        _ => {
            return Err(SelectedIntegrity(
                "incompatible_index: corrupt semantic symbol scope/document".into(),
            )
            .into());
        }
    };
    Ok(sem::SymbolKey {
        scheme: semantic_enum(scheme)?,
        symbol,
        scope,
        document,
    })
}
fn semantic_symbol_keys(
    db: &Connection,
    table: &str,
    parent: &str,
    id: i64,
    source_set: &str,
) -> Result<Vec<crate::semantic_evidence::SymbolKey>> {
    let mut stmt=db.prepare(&format!("SELECT ordinal,scheme,symbol,scope,document_language,document_path FROM {table} WHERE {parent}=?1 ORDER BY ordinal"))?;
    let rows = stmt
        .query_map([id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut keys = Vec::new();
    for (ordinal, scheme, symbol, scope, language, path) in rows {
        selected_ensure!(
            ordinal == keys.len() as i64,
            "incompatible_index: corrupt semantic symbol ordinal"
        );
        keys.push(semantic_symbol_key_read(
            scheme, symbol, scope, language, path, source_set,
        )?);
    }
    Ok(keys)
}
fn semantic_candidate_ids(
    db: &Connection,
    table: &str,
    parent: i64,
    kind: &str,
) -> Result<Vec<String>> {
    let sql =
        format!("SELECT ordinal,candidate_id FROM {table} WHERE binding_id=?1 ORDER BY ordinal");
    let rows = db
        .prepare(&sql)?
        .query_map([parent], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut ids = Vec::new();
    for (ordinal, bytes) in rows {
        selected_ensure!(
            ordinal == ids.len() as i64,
            "incompatible_index: corrupt semantic join ordinal"
        );
        ids.push(
            if kind == "declaration" {
                id_codec::decode_sid(&bytes)
            } else {
                id_codec::decode_occ(&bytes)
            }
            .context(SelectedIntegrity(
                "incompatible_index: corrupt semantic join candidate ID".into(),
            ))?,
        );
    }
    Ok(ids)
}

fn read_semantic(db: &Connection, key: &str) -> Result<crate::semantic_evidence::SemanticSnapshot> {
    read_semantic_tuple(db, key, None, None, false)
}

fn read_semantic_tuple(
    db: &Connection,
    key: &str,
    selected_tuple: Option<i64>,
    history_syntax: Option<&str>,
    trusted_admitted_raw: bool,
) -> Result<crate::semantic_evidence::SemanticSnapshot> {
    read_semantic_tuple_for_call(
        db,
        key,
        selected_tuple,
        history_syntax,
        trusted_admitted_raw,
        None,
    )
}

fn read_semantic_tuple_for_call(
    db: &Connection,
    key: &str,
    selected_tuple: Option<i64>,
    history_syntax: Option<&str>,
    trusted_admitted_raw: bool,
    call_id: Option<&[u8]>,
) -> Result<crate::semantic_evidence::SemanticSnapshot> {
    use crate::native_evidence::DocumentKey;
    use crate::semantic_evidence as sem;
    let (source_set, native_id): (String, String) = db.query_row(
        "SELECT source_set_id,native_revision_id FROM native_revisions WHERE id=?1",
        [key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut result = sem::SemanticSnapshot {
        producers: vec![],
        coverage: vec![],
        provenance: vec![],
        symbols: vec![],
        declaration_bindings: vec![],
        references: vec![],
        call_bindings: vec![],
        type_relationships: vec![],
    };
    let descriptors=db.prepare("SELECT p.id,p.version,p.executable_hash,p.kind,p.position_encoding FROM semantic_revision_producers h JOIN semantic_producers p ON p.id=h.id AND p.version=h.version WHERE h.revision_id=?1 AND (?2 IS NULL OR EXISTS(SELECT 1 FROM semantic_coverage c WHERE c.tuple_id=?2 AND c.revision_id=h.revision_id AND c.producer_id=h.id AND c.producer_version=h.version)) ORDER BY p.id,p.version")?.query_map(params![key, selected_tuple],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, version, executable_hash, kind, encoding) in descriptors {
        let languages = semantic_ordered(
            db,
            "SELECT ordinal,language FROM semantic_producer_languages WHERE id=?1 AND version=?2 ORDER BY ordinal",
            &[&id, &version],
        )?;
        result.producers.push(sem::Producer {
            id,
            version,
            executable_hash,
            kind: semantic_enum(kind)?,
            languages,
            position_encoding: semantic_enum(encoding)?,
        });
    }
    let tuples=db.prepare("SELECT tuple_id,producer_id,producer_version,language,path,requested,selected,state,diagnostic FROM semantic_coverage WHERE revision_id=?1 AND (?2 IS NULL OR tuple_id=?2) ORDER BY tuple_id")?.query_map(params![key, selected_tuple],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,bool>(5)?,r.get::<_,bool>(6)?,r.get::<_,String>(7)?,r.get::<_,Option<String>>(8)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (tuple, producer_id, _version, language, path, requested, selected, state, diagnostic) in
        tuples
    {
        let supported_roles = semantic_ordered(
            db,
            "SELECT ordinal,role FROM semantic_coverage_roles WHERE tuple_id=?1 AND kind='supported' ORDER BY ordinal",
            &[&tuple],
        )?;
        let observed_roles = semantic_ordered(
            db,
            "SELECT ordinal,role FROM semantic_coverage_roles WHERE tuple_id=?1 AND kind='observed' ORDER BY ordinal",
            &[&tuple],
        )?;
        result.coverage.push(sem::Coverage {
            producer_id: producer_id.clone(),
            language: semantic_enum(language.clone())?,
            source_set_id: source_set.clone(),
            document_path: path.clone(),
            revision_id: native_id.clone(),
            requested,
            selected,
            state: semantic_enum(state)?,
            supported_roles,
            observed_roles,
            diagnostic,
        });
        let document = DocumentKey {
            source_set_id: source_set.clone(),
            language: language.clone(),
            path: path.clone(),
        };
        let rows=db.prepare("SELECT p.provenance_id,p.content_hash,p.evidence_kind,p.freshness,p.derived_from_provenance_id,b.producer_id,b.producer_version,b.producer_hash,b.artifact_hash,b.language,b.source_set_id,b.captured_native_revision_id,b.source_manifest_hash,b.toolchain_hash,b.config_hash,b.dependency_hash,b.basis_id,p.capture_id,p.derived_from_record_kind,p.derived_from_fact_digest FROM semantic_provenance p JOIN semantic_basis b ON b.tuple_id=p.tuple_id AND b.basis_id=p.basis_id WHERE p.tuple_id=?1 AND (?5 IS NULL OR p.provenance_id IN (SELECT provenance_id FROM semantic_call_bindings WHERE tuple_id=?1 AND call_id=?5)) AND (?2 IS NULL OR p.provenance_id IN (SELECT provenance_id FROM semantic_declaration_bindings WHERE tuple_id=?1 AND syntax_id=?2 UNION SELECT s.provenance_id FROM semantic_symbols s JOIN semantic_symbol_targets t ON t.symbol_id=s.symbol_id WHERE s.tuple_id=?1 AND t.kind='internal' AND t.internal_syntax_id=?2 AND t.internal_language=?3 AND t.internal_path=?4 UNION SELECT provenance_id FROM semantic_type_relationships WHERE tuple_id=?1 AND source_kind='internal' AND source_internal_syntax_id=?2 AND source_internal_language=?3 AND source_internal_path=?4)) ORDER BY p.rowid")?.query_map(params![tuple, history_syntax.and_then(id_codec::encode_sid), language, path, call_id],|r|{
            Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,String>(8)?,r.get::<_,String>(9)?,r.get::<_,String>(10)?,r.get::<_,String>(11)?,r.get::<_,String>(12)?,r.get::<_,String>(13)?,r.get::<_,String>(14)?,r.get::<_,String>(15)?,r.get::<_,i64>(16)?,r.get::<_,Option<i64>>(17)?,r.get::<_,Option<String>>(18)?,r.get::<_,Option<String>>(19)?))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        for (
            id,
            content_hash,
            evidence_kind,
            freshness,
            derived,
            producer,
            version,
            producer_hash,
            artifact_hash,
            basis_language,
            basis_source_set,
            basis_revision,
            source_manifest_hash,
            toolchain_hash,
            config_hash,
            dependency_hash,
            basis_id,
            capture_id,
            record_kind,
            fact_digest,
        ) in rows
        {
            let derived_from = match (capture_id, derived, record_kind, fact_digest) {
                (None, None, None, None) => None,
                (Some(capture), Some(proof), Some(kind), Some(digest)) => {
                    type RawRow = (
                        String,
                        String,
                        String,
                        String,
                        Option<Vec<u8>>,
                        String,
                        String,
                        String,
                        String,
                    );
                    let raw: Option<RawRow> = db.query_row(
                        "SELECT p.record_kind,p.fact_digest,p.language,p.path,CASE WHEN ?3 THEN NULL ELSE p.canonical_bytes END,c.producer_id,c.source_set_id,c.captured_revision,c.artifact_hash FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE p.capture_id=?1 AND p.proof_id=?2",
                        params![capture, proof, trusted_admitted_raw],
                        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?)),
                    ).optional()?;
                    let (
                        stored_kind,
                        stored_digest,
                        stored_language,
                        stored_path,
                        canonical,
                        stored_producer,
                        stored_source_set,
                        stored_revision,
                        stored_artifact,
                    ) = raw.context(SelectedIntegrity(
                        "incompatible_index: missing raw proof".into(),
                    ))?;
                    if let Some(canonical) = canonical.as_deref() {
                        let envelope: serde_json::Value = serde_json::from_slice(canonical)
                            .context(SelectedIntegrity(
                                "incompatible_index: corrupt raw envelope".into(),
                            ))?;
                        selected_ensure!(
                            envelope.get("recordKind").and_then(|v| v.as_str())
                                == Some(kind.as_str())
                                && envelope.get("relativePath").and_then(|v| v.as_str())
                                    == Some(path.as_str())
                                && envelope.get("formatVersion").and_then(|v| v.as_u64())
                                    == Some(1),
                            "incompatible_index: corrupt raw envelope identity"
                        );
                    } else {
                        selected_ensure!(
                            trusted_admitted_raw,
                            "incompatible_index: missing raw envelope"
                        );
                    }
                    selected_ensure!(
                        stored_kind == kind
                            && stored_digest == digest
                            && stored_language == language
                            && stored_path == path
                            && stored_producer == producer_id
                            && stored_source_set == source_set
                            && stored_revision == basis_revision
                            && stored_artifact == artifact_hash
                            && crate::native_ids::digest(
                                b"trellis.capture-proof.v1\0",
                                &crate::native_ids::canonical(
                                    &serde_json::json!({"producerId":stored_producer,"artifactHash":stored_artifact,"sourceSetId":stored_source_set,"sourceRevision":stored_revision,"language":stored_language,"relativePath":stored_path,"factDigest":stored_digest})
                                )
                            ) == proof
                            && (trusted_admitted_raw
                                || canonical.as_deref().is_some_and(|bytes| {
                                    crate::native_ids::digest(b"trellis.semantic-fact.v1\0", bytes)
                                        == digest
                                })),
                        "incompatible_index: corrupt raw proof binding"
                    );
                    Some(sem::DerivedFrom {
                        provenance_id: proof,
                        record_kind: semantic_enum(kind)?,
                        fact_digest: digest,
                    })
                }
                _ => {
                    return Err(SelectedIntegrity(
                        "incompatible_index: corrupt raw proof link".into(),
                    )
                    .into());
                }
            };
            let lookup_dependencies = semantic_ordered::<String>(
                db,
                "SELECT ordinal,lookup_key FROM semantic_basis_lookup_dependencies WHERE basis_id=?1 ORDER BY ordinal",
                &[&basis_id],
            )?;
            result.provenance.push(sem::Provenance {
                id,
                producer_id: producer_id.clone(),
                document: document.clone(),
                revision_id: native_id.clone(),
                content_hash,
                evidence_kind: semantic_enum(evidence_kind)?,
                basis: Some(sem::SemanticBasis {
                    producer_id: producer,
                    producer_version: version,
                    producer_hash,
                    artifact_hash,
                    language: semantic_enum(basis_language)?,
                    source_set_id: basis_source_set,
                    revision_id: basis_revision,
                    source_manifest_hash,
                    toolchain_hash,
                    config_hash,
                    dependency_hash,
                    lookup_dependencies,
                }),
                freshness: semantic_enum(freshness)?,
                derived_from,
            });
        }
        if history_syntax.is_none() {
            read_semantic_facts(db, tuple, &document, &native_id, &mut result, call_id)?;
        }
    }
    Ok(result)
}

fn read_semantic_history(
    db: &Connection,
    key: &str,
    index_revision: u64,
    producer_id: &str,
    document: &crate::native_evidence::DocumentKey,
    syntax_id: &str,
) -> Result<Option<SemanticHistory>> {
    use crate::semantic_evidence::{CoverageState, Freshness};
    let source_set: String = db.query_row(
        "SELECT source_set_id FROM native_revisions WHERE id=?1",
        [key],
        |r| r.get(0),
    )?;
    if source_set != document.source_set_id {
        return Ok(None);
    }
    let selected_tuple: Option<i64> = db.query_row(
        "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
        params![key, producer_id, document.language, document.path], |r| r.get(0),
    ).optional()?;
    let Some(selected_tuple) = selected_tuple else {
        return Ok(None);
    };
    let selected_snapshot = read_semantic_tuple(db, key, Some(selected_tuple), None, false)?;
    let [selected_coverage] = <[_; 1]>::try_from(selected_snapshot.coverage).map_err(|_| {
        SelectedIntegrity("incompatible_index: corrupt selected semantic coverage".into())
    })?;
    let state: CoverageState = selected_coverage.state;
    if !matches!(state, CoverageState::Failed | CoverageState::Omitted) {
        return Ok(None);
    }
    selected_ensure!(
        selected_snapshot.provenance.is_empty()
            && selected_snapshot.symbols.is_empty()
            && selected_snapshot.declaration_bindings.is_empty()
            && selected_snapshot.references.is_empty()
            && selected_snapshot.call_bindings.is_empty()
            && selected_snapshot.type_relationships.is_empty(),
        "incompatible_index: corrupt unselected semantic tuple contains facts"
    );
    let selected_document: Option<String> = db.query_row(
        "SELECT v.content_hash FROM revision_documents d JOIN document_versions v ON v.vid=d.vid WHERE d.revision_id=?1 AND d.language=?2 AND d.path=?3",
        params![key, document.language, document.path], |r| r.get(0),
    ).optional()?;
    let selected_header: (String, String, String, String) = db.query_row(
        "SELECT native_revision_id,toolchain_hash,config_hash,dependency_hash FROM native_revisions WHERE id=?1",
        [key], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )?;
    let selected_descriptor: Option<(String, String)> = db.query_row(
        "SELECT p.version,p.executable_hash FROM semantic_requested_producers p JOIN semantic_requested_producer_languages l ON l.revision_id=p.revision_id AND l.id=p.id WHERE p.revision_id=?1 AND p.id=?2 AND l.language=?3",
        params![key, producer_id, document.language], |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    // published_index_revision orders storage pins; never order immutable IDs or
    // fall through to an older tuple when the newest eligible one has no fact.
    let previous: Option<(String, i64)> = db.query_row(
        "SELECT r.id,c.tuple_id FROM semantic_coverage c JOIN native_revisions r ON r.id=c.revision_id JOIN native_revision_supersessions s ON s.revision_id=r.id AND s.state='retained' WHERE c.producer_id=?1 AND c.language=?2 AND c.path=?3 AND r.source_set_id=?4 AND c.state IN ('complete','partial') AND r.published_index_revision<?5 AND NOT EXISTS (SELECT 1 FROM revision_capture_inputs i WHERE i.revision_id=r.id AND i.input_key IN ('__released:v1','__pending_release:v1')) ORDER BY r.published_index_revision DESC LIMIT 1",
        params![producer_id, document.language, document.path, document.source_set_id, index_revision as i64],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    let Some((previous_key, tuple)) = previous else {
        return Ok(None);
    };
    let snapshot = read_semantic_tuple(db, &previous_key, Some(tuple), Some(syntax_id), false)?;
    let [coverage] = <[_; 1]>::try_from(snapshot.coverage).map_err(|_| {
        SelectedIntegrity("incompatible_index: corrupt semantic history coverage".into())
    })?;
    let provenance = snapshot.provenance;
    let historical_descriptor = snapshot
        .producers
        .first()
        .map(|p| (&p.version, &p.executable_hash));
    let historical_bases = db.prepare("SELECT producer_version,producer_hash,source_set_id,captured_native_revision_id,source_manifest_hash,toolchain_hash,config_hash,dependency_hash FROM semantic_basis WHERE tuple_id=?1")?
        .query_map([tuple], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get::<_, String>(3)?,r.get::<_, String>(4)?,r.get::<_, String>(5)?,r.get::<_, String>(6)?,r.get::<_, String>(7)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let rows = db.prepare("SELECT d.ordinal,d.language,d.path,v.content_hash FROM revision_documents d JOIN document_versions v ON v.vid=d.vid WHERE d.revision_id=?1 ORDER BY d.ordinal")?
        .query_map([key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut manifest = Vec::with_capacity(rows.len());
    for (ordinal, language, path, content_hash) in rows {
        selected_ensure!(
            ordinal == manifest.len() as i64,
            "incompatible_index: corrupt semantic history manifest ordinal"
        );
        manifest.push(serde_json::json!({"document":{"sourceSetId":document.source_set_id,"language":language,"path":path},"contentHash":content_hash}));
    }
    let manifest_hash = crate::native_ids::digest(
        b"",
        &crate::native_ids::canonical(&serde_json::json!(manifest)),
    );
    let historical_document_hash: String = db.query_row(
        "SELECT v.content_hash FROM revision_documents d JOIN document_versions v ON v.vid=d.vid WHERE d.revision_id=?1 AND d.language=?2 AND d.path=?3",
        params![previous_key, document.language, document.path], |r| r.get(0),
    )?;
    let presentation_freshness = if selected_document.as_ref() != Some(&historical_document_hash) {
        Freshness::Stale
    } else if historical_bases.iter().any(
        |(version, hash, source_set, revision, manifest, toolchain, config, dependency)| {
            selected_descriptor
                .as_ref()
                .is_none_or(|(current_version, current_hash)| {
                    version != current_version || hash != current_hash
                })
                || source_set != &document.source_set_id
                || revision != &selected_header.0
                || manifest != &manifest_hash
                || toolchain != &selected_header.1
                || config != &selected_header.2
                || dependency != &selected_header.3
        },
    ) || (historical_bases.is_empty()
        && (coverage.revision_id != selected_header.0
            || selected_descriptor
                .as_ref()
                .is_none_or(|(version, hash)| historical_descriptor != Some((version, hash)))))
    {
        Freshness::PossiblyStale
    } else {
        Freshness::Fresh
    };
    Ok(Some(SemanticHistory {
        coverage,
        provenance,
        presentation_freshness,
    }))
}

fn read_semantic_facts(
    db: &Connection,
    tuple: i64,
    document: &crate::native_evidence::DocumentKey,
    native_id: &str,
    result: &mut crate::semantic_evidence::SemanticSnapshot,
    selected_call: Option<&[u8]>,
) -> Result<()> {
    use crate::semantic_evidence as sem;
    let source_set = &document.source_set_id;
    if selected_call.is_some() {
        return read_semantic_call_facts(db, tuple, document, result, selected_call);
    }
    let symbol_rows=db.prepare("SELECT symbol_id,ordinal,key_scheme,key_symbol,key_scope,key_document_language,key_document_path,display_name,provenance_id FROM semantic_symbols WHERE tuple_id=?1 ORDER BY ordinal")?.query_map([tuple],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,Option<String>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,String>(8)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (
        ordinal,
        (id, stored, scheme, symbol, scope, language, path, display_name, provenance_id),
    ) in symbol_rows.into_iter().enumerate()
    {
        selected_ensure!(
            ordinal as i64 == stored,
            "incompatible_index: corrupt semantic symbol ordinal"
        );
        result.symbols.push(sem::Symbol {
            key: semantic_symbol_key_read(scheme, symbol, scope, language, path, source_set)?,
            display_name,
            declarations: semantic_targets(
                db,
                "semantic_symbol_targets",
                "symbol_id=?1",
                &[&id],
                source_set,
            )?,
            provenance_id,
        });
    }
    let binding_rows=db.prepare("SELECT binding_id,ordinal,syntax_id,provenance_id,join_document_language,join_document_path,join_native_revision_id,join_content_hash,join_start,join_end,join_kind,join_status,join_diagnostic FROM semantic_declaration_bindings WHERE tuple_id=?1 ORDER BY ordinal")?.query_map([tuple],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<Vec<u8>>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?,r.get::<_,i64>(8)?,r.get::<_,i64>(9)?,r.get::<_,String>(10)?,r.get::<_,String>(11)?,r.get::<_,Option<String>>(12)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (
        ordinal,
        (
            id,
            stored,
            syntax,
            provenance_id,
            language,
            path,
            revision_id,
            content_hash,
            start,
            end,
            kind,
            status,
            diagnostic,
        ),
    ) in binding_rows.into_iter().enumerate()
    {
        selected_ensure!(
            ordinal as i64 == stored,
            "incompatible_index: corrupt semantic declaration ordinal"
        );
        result.declaration_bindings.push(sem::DeclarationBinding {
            syntax_id: syntax
                .map(|id| {
                    id_codec::decode_sid(&id).context(SelectedIntegrity(
                        "incompatible_index: corrupt semantic syntax ID".into(),
                    ))
                })
                .transpose()?,
            symbols: semantic_symbol_keys(
                db,
                "semantic_declaration_binding_symbols",
                "binding_id",
                id,
                source_set,
            )?,
            join: sem::Join {
                anchor: sem::MeasuredAnchor {
                    document: crate::native_evidence::DocumentKey {
                        source_set_id: source_set.clone(),
                        language,
                        path,
                    },
                    revision_id,
                    content_hash,
                    range: crate::native_evidence::Range {
                        start: start.try_into().context(SelectedIntegrity(
                            "incompatible_index: invalid semantic start".into(),
                        ))?,
                        end: end.try_into().context(SelectedIntegrity(
                            "incompatible_index: invalid semantic end".into(),
                        ))?,
                    },
                    kind: semantic_enum(kind)?,
                },
                status: semantic_enum(status)?,
                candidate_ids: semantic_candidate_ids(
                    db,
                    "semantic_declaration_join_candidates",
                    id,
                    "declaration",
                )?,
                diagnostic,
            },
            provenance_id,
        });
    }
    let reference_rows = db
        .prepare("SELECT * FROM semantic_references WHERE tuple_id=?1 ORDER BY rowid")?
        .query_map([tuple], |r| {
            let target = semantic_target_read(r, 10, source_set).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    10,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(e.to_string())),
                )
            })?;
            Ok((
                r.get::<_, Vec<u8>>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Vec<u8>>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                target,
                r.get::<_, String>(20)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (
        id,
        ordinal,
        owner,
        start,
        end,
        spelling,
        lookup_key,
        site,
        resolution,
        declared_target,
        provenance_id,
    ) in reference_rows
    {
        let roles = semantic_ordered(
            db,
            "SELECT ordinal,role FROM semantic_reference_roles WHERE tuple_id=?1 AND reference_id=?2 ORDER BY ordinal",
            &[&tuple, &id],
        )?;
        let candidates = semantic_targets(
            db,
            "semantic_reference_candidates",
            "tuple_id=?1 AND reference_id=?2",
            &[&tuple, &id],
            source_set,
        )?;
        result.references.push(sem::Reference {
            id: id_codec::decode_occ(&id).context(SelectedIntegrity(
                "incompatible_index: corrupt semantic reference ID".into(),
            ))?,
            owner_syntax_id: id_codec::decode_sid(&owner).context(SelectedIntegrity(
                "incompatible_index: corrupt semantic owner ID".into(),
            ))?,
            ordinal: ordinal.try_into().context(SelectedIntegrity(
                "incompatible_index: invalid semantic ordinal".into(),
            ))?,
            document: document.clone(),
            revision_id: native_id.into(),
            range: crate::native_evidence::Range {
                start: start.try_into().context(SelectedIntegrity(
                    "incompatible_index: invalid semantic start".into(),
                ))?,
                end: end.try_into().context(SelectedIntegrity(
                    "incompatible_index: invalid semantic end".into(),
                ))?,
            },
            spelling,
            lookup_key,
            site: semantic_enum(site)?,
            roles,
            resolution: semantic_enum(resolution)?,
            declared_target,
            candidates,
            provenance_id,
        });
    }
    read_semantic_call_facts(db, tuple, document, result, None)?;
    let relationship_rows = db
        .prepare("SELECT * FROM semantic_type_relationships WHERE tuple_id=?1 ORDER BY ordinal")?
        .query_map([tuple], |r| {
            let source = semantic_target_read(r, 4, source_set).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(e.to_string())),
                )
            })?;
            let target = semantic_target_read(r, 14, source_set).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(e.to_string())),
                )
            })?;
            Ok((
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                source,
                target,
                r.get::<_, String>(24)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (ordinal, (stored, kind, source, target, provenance_id)) in
        relationship_rows.into_iter().enumerate()
    {
        selected_ensure!(
            ordinal as i64 == stored,
            "incompatible_index: corrupt semantic relationship ordinal"
        );
        result.type_relationships.push(sem::TypeRelationship {
            kind: semantic_enum(kind)?,
            source: source.context(SelectedIntegrity(
                "incompatible_index: missing semantic source".into(),
            ))?,
            target: target.context(SelectedIntegrity(
                "incompatible_index: missing semantic target".into(),
            ))?,
            provenance_id,
        });
    }
    Ok(())
}

fn read_semantic_call_facts(
    db: &Connection,
    tuple: i64,
    document: &crate::native_evidence::DocumentKey,
    result: &mut crate::semantic_evidence::SemanticSnapshot,
    selected_call: Option<&[u8]>,
) -> Result<()> {
    use crate::semantic_evidence as sem;
    let source_set = &document.source_set_id;
    let call_rows = db
        .prepare("SELECT * FROM semantic_call_bindings WHERE tuple_id=?1 AND (?2 IS NULL OR call_id=?2) ORDER BY ordinal")?
        .query_map(params![tuple, selected_call], |r| {
            let target = semantic_target_read(r, 14, source_set).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    14,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::other(e.to_string())),
                )
            })?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, i64>(8)?,
                r.get::<_, i64>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, String>(11)?,
                r.get::<_, Option<String>>(12)?,
                r.get::<_, String>(13)?,
                target,
                r.get::<_, String>(24)?,
                r.get::<_, bool>(25)?,
                r.get::<_, Option<bool>>(26)?,
                r.get::<_, String>(27)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (
        ordinal,
        (
            id,
            stored,
            call_id,
            language,
            path,
            revision_id,
            content_hash,
            start,
            end,
            kind,
            status,
            diagnostic,
            resolution,
            declared_target,
            dispatch,
            possible_dispatch_complete,
            stale_target,
            provenance_id,
        ),
    ) in call_rows.into_iter().enumerate()
    {
        selected_ensure!(
            selected_call.is_some() || ordinal as i64 == stored,
            "incompatible_index: corrupt semantic call ordinal"
        );
        result.call_bindings.push(sem::CallBinding {
            call_id: call_id
                .map(|id| {
                    id_codec::decode_occ(&id).context(SelectedIntegrity(
                        "incompatible_index: corrupt semantic call ID".into(),
                    ))
                })
                .transpose()?,
            join: sem::Join {
                anchor: sem::MeasuredAnchor {
                    document: crate::native_evidence::DocumentKey {
                        source_set_id: source_set.clone(),
                        language,
                        path,
                    },
                    revision_id,
                    content_hash,
                    range: crate::native_evidence::Range {
                        start: start.try_into().context(SelectedIntegrity(
                            "incompatible_index: invalid semantic start".into(),
                        ))?,
                        end: end.try_into().context(SelectedIntegrity(
                            "incompatible_index: invalid semantic end".into(),
                        ))?,
                    },
                    kind: semantic_enum(kind)?,
                },
                status: semantic_enum(status)?,
                candidate_ids: semantic_candidate_ids(
                    db,
                    "semantic_call_join_candidates",
                    id,
                    "call",
                )?,
                diagnostic,
            },
            resolution: semantic_enum(resolution)?,
            declared_target,
            candidates: semantic_targets(
                db,
                "semantic_call_candidates",
                "binding_id=?1",
                &[&id],
                source_set,
            )?,
            dispatch: semantic_enum(dispatch)?,
            possible_dispatch: semantic_targets(
                db,
                "semantic_call_possible_dispatch",
                "binding_id=?1",
                &[&id],
                source_set,
            )?,
            possible_dispatch_complete,
            stale_target,
            provenance_id,
        });
    }
    Ok(())
}

/// The pinned identity kind of a producer id; every producer outside the synthetic pin is an executable.
fn producer_identity_kind(producer_id: &str) -> &'static str {
    crate::scip_admission::pinned_producer_by_id(producer_id)
        .map_or(
            crate::semantic_batch::descriptor::IdentityKind::Executable,
            |pinned| pinned.identity_kind,
        )
        .as_str()
}

fn write_import_semantic(
    db: &Connection,
    key: &str,
    publication: &crate::semantic_evidence::SemanticPublication,
    captured: &crate::semantic_evidence::CapturedComponents,
) -> Result<()> {
    for producer in &publication.producers {
        db.execute(
            "INSERT OR IGNORE INTO semantic_producers VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                producer.id,
                producer.version,
                producer.executable_hash,
                semantic_variant(&producer.kind)?,
                semantic_variant(&producer.position_encoding)?,
                producer_identity_kind(&producer.id)
            ],
        )?;
        for (ordinal, language) in producer.languages.iter().enumerate() {
            db.execute(
                "INSERT OR IGNORE INTO semantic_producer_languages VALUES(?1,?2,?3,?4)",
                params![
                    producer.id,
                    producer.version,
                    semantic_variant(language)?,
                    ordinal as i64
                ],
            )?;
        }
        db.execute(
            "INSERT OR REPLACE INTO semantic_revision_producers VALUES(?1,?2,?3)",
            params![key, producer.id, producer.version],
        )?;
    }
    for producer in &captured.requested_producers {
        if publication.producers.iter().any(|p| p.id == producer.id) {
            db.execute(
                "INSERT INTO semantic_requested_producers VALUES(?1,?2,?3,?4)",
                params![key, producer.id, producer.version, producer.executable_hash],
            )?;
            for (ordinal, language) in producer.languages.iter().enumerate() {
                db.execute(
                    "INSERT INTO semantic_requested_producer_languages VALUES(?1,?2,?3,?4)",
                    params![
                        key,
                        producer.id,
                        semantic_variant(language)?,
                        ordinal as i64
                    ],
                )?;
            }
        }
    }
    write_import_coverage(db, key, publication)
}

/// Writes every coverage tuple of `publication` with its owned semantic rows.
fn write_import_coverage(
    db: &Connection,
    key: &str,
    publication: &crate::semantic_evidence::SemanticPublication,
) -> Result<()> {
    for coverage in &publication.coverage {
        let producer = publication
            .producers
            .iter()
            .find(|p| p.id == coverage.producer_id)
            .context("missing semantic producer")?;
        execute_cached(
            db,
            "INSERT INTO semantic_coverage(revision_id,producer_id,producer_version,language,path,requested,selected,state,diagnostic) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                key,
                coverage.producer_id,
                producer.version,
                semantic_variant(&coverage.language)?,
                coverage.document_path,
                coverage.requested,
                coverage.selected,
                semantic_variant(&coverage.state)?,
                coverage.diagnostic
            ],
        )?;
        let tuple = db.last_insert_rowid();
        for (kind, roles) in [
            ("supported", &coverage.supported_roles),
            ("observed", &coverage.observed_roles),
        ] {
            for (ordinal, role) in roles.iter().enumerate() {
                execute_cached(
                    db,
                    "INSERT INTO semantic_coverage_roles VALUES(?1,?2,?3,?4)",
                    params![tuple, kind, ordinal as i64, semantic_variant(role)?],
                )?;
            }
        }
        write_semantic_tuple(db, tuple, coverage, publication)?;
    }
    Ok(())
}

/// Faults the store-level batch publication tests inject.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchPublishFault {
    /// A forced SQL error inside the transaction.
    SqlFault,
    /// The cancel flag is raised after the last statement, before commit.
    PrecommitCancel,
    /// COMMIT succeeds but its result is reported lost.
    CommitLostAfterCommit,
    /// COMMIT never happens and its result is reported lost.
    CommitLostBeforeCommit,
    /// COMMIT fails with SQLite contention and the transaction rolls back.
    CommitBusyBeforeCommit,
    /// An INSERT inside the transaction hits SQLITE_FULL.
    InsertDiskFull,
}

/// What a durable re-read says about a batch COMMIT whose result was not seen.
enum BatchCommitState {
    Committed,
    RolledBack,
    Moved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
enum CommitLoss {
    After,
    Before,
    BusyBefore,
    DiskFullBefore,
}

/// Limits the publication measures disk growth against.
fn batch_publication_limits() -> crate::semantic_batch::BatchLimits {
    #[cfg(feature = "test-support")]
    {
        crate::semantic_batch::BatchLimits::effective()
            .unwrap_or(crate::semantic_batch::BatchLimits::PRODUCTION)
    }
    #[cfg(not(feature = "test-support"))]
    {
        crate::semantic_batch::BatchLimits::PRODUCTION
    }
}

/// `file:` URI that attaches `stage.db` read-only.
fn stage_attach_uri(path: &Path) -> String {
    use std::fmt::Write as _;
    let mut uri = String::from("file:");
    for byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(*byte as char);
        } else {
            let _ = write!(uri, "%{byte:02X}");
        }
    }
    uri.push_str("?mode=ro");
    uri
}

/// Detaches the batch stage on every exit. Declared before the transaction so it drops after
/// the rollback or commit, which is the only time SQLite permits DETACH.
struct StageAttachment<'a>(&'a Connection);
impl Drop for StageAttachment<'_> {
    fn drop(&mut self) {
        let _ = self.0.execute_batch("DETACH DATABASE stage");
    }
}

/// Fills a zero-filled capture blob from the staged artifact file in `IO_BUFFER_BYTES` chunks.
fn fill_capture_blob(
    db: &Connection,
    capture_id: i64,
    path: &Path,
    length: i64,
    cancel_between_chunks: &dyn Fn() -> Result<()>,
) -> Result<()> {
    use std::io::{Read, Write};
    let mut file = std::fs::File::open(path).context("staged artifact")?;
    ensure!(
        i64::try_from(file.metadata()?.len())? == length,
        "staged artifact length differs"
    );
    let mut blob = db.blob_open(
        "main",
        "semantic_raw_captures",
        "binary_bytes",
        capture_id,
        false,
    )?;
    let mut buffer = vec![0u8; crate::semantic_batch::limits::IO_BUFFER_BYTES as usize];
    let mut written = 0i64;
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        blob.write_all(&buffer[..read])?;
        written += read as i64;
        cancel_between_chunks()?;
    }
    ensure!(written == length, "staged artifact length differs");
    Ok(())
}

/// Raw rows of a batch. The document and proof statements join the ordinal-to-capture pairs of
/// `with_capture_ids`; `CROSS JOIN` keeps the staged table as the driver, so every select reads in
/// the order that table is stored and needs no sorter.
const RAW_DOCUMENTS_SQL: &str = "INSERT OR IGNORE INTO semantic_raw_documents SELECT m.capture_id,s.language,s.path,s.content_hash,d.wire_digest,d.capped FROM stage.documents d CROSS JOIN stage.sources s ON s.ord=d.source_ord CROSS JOIN capture_ids m ON m.ordinal=d.artifact ORDER BY d.source_ord";
const RAW_EXCLUSIONS_SQL: &str = "INSERT OR IGNORE INTO semantic_raw_exclusions SELECT ?1,s.language,s.path,c.exclusion FROM stage.coverage c JOIN stage.sources s ON s.ord=c.source_ord WHERE c.exclusion IS NOT NULL ORDER BY c.source_ord";
const RAW_PROOFS_SQL: &str = "INSERT OR IGNORE INTO semantic_raw_proofs SELECT m.capture_id,c.proof_id,c.record_kind,c.fact_digest,s.language,s.path,c.canonical_bytes FROM stage.claims c CROSS JOIN stage.sources s ON s.ord=c.source_ord CROSS JOIN capture_ids m ON m.ordinal=c.artifact ORDER BY c.claim_id";

fn with_capture_ids(values: &str, statement: &str) -> String {
    format!("WITH capture_ids(ordinal,capture_id) AS (VALUES {values}) {statement}")
}

/// The statements batch publication runs against the attached stage, for query-plan tests.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn batch_publication_statements_for_tests() -> Vec<String> {
    let mut statements = vec![
        with_capture_ids("(0,1)", RAW_DOCUMENTS_SQL),
        RAW_EXCLUSIONS_SQL.to_owned(),
        with_capture_ids("(0,1)", RAW_PROOFS_SQL),
    ];
    statements.extend(crate::semantic_import::staged_publication_statements(
        "stage.",
    ));
    statements
}

/// Writes the batch row, one capture per staged artifact, and the document, exclusion and proof
/// rows. Rows of a batch that is already stored are refilled and never compared. Returns the
/// batch id.
fn write_staged_raw_rows(
    db: &Connection,
    stage: &crate::semantic_batch::staging::Stage,
    descriptor: &crate::semantic_batch::descriptor::Descriptor,
    descriptor_bytes: &[u8],
    descriptor_sha256: &str,
    gate: &dyn Fn() -> Result<()>,
) -> Result<i64> {
    let producer_id = &descriptor.producer.name;
    let source_set_id = &descriptor.source_set.id;
    let captured_revision = &descriptor.source_revision;
    db.execute("INSERT INTO semantic_raw_batches(producer_id,producer_version,profile_id,source_set_id,captured_revision,source_manifest_hash,descriptor_sha256,descriptor_bytes,decoder_version) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9) ON CONFLICT(producer_id,descriptor_sha256,captured_revision) DO NOTHING", params![producer_id,descriptor.producer.version,descriptor.profile_id,source_set_id,captured_revision,descriptor.source_manifest_hash,descriptor_sha256,descriptor_bytes,descriptor.decoder_version])?;
    let batch_id: i64 = db.query_row("SELECT batch_id FROM semantic_raw_batches WHERE producer_id=?1 AND descriptor_sha256=?2 AND captured_revision=?3", params![producer_id,descriptor_sha256,captured_revision], |r| r.get(0))?;
    let artifacts: Vec<(i64, String, String, i64)> = db
        .prepare(
            "SELECT ordinal,language,sha256,byte_length FROM stage.artifacts ORDER BY ordinal",
        )?
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(!artifacts.is_empty(), "staged batch has no artifacts");
    let mut capture_ids = Vec::with_capacity(artifacts.len());
    for (ordinal, language, sha256, length) in &artifacts {
        let other: Option<i64> = db.query_row("SELECT batch_id FROM semantic_raw_captures WHERE producer_id=?1 AND artifact_hash=?2 AND source_set_id=?3 AND captured_revision=?4", params![producer_id,sha256,source_set_id,captured_revision], |r| r.get(0)).optional()?;
        ensure!(
            other.is_none_or(|other| other == batch_id),
            "invalid_request: artifact already captured by another batch"
        );
        let existing: Option<i64> = db
            .query_row(
                "SELECT capture_id FROM semantic_raw_captures WHERE batch_id=?1 AND ordinal=?2",
                params![batch_id, ordinal],
                |r| r.get(0),
            )
            .optional()?;
        let capture_id = if let Some(existing) = existing {
            existing
        } else {
            db.execute("INSERT INTO semantic_raw_captures(batch_id,ordinal,language,producer_id,artifact_hash,source_set_id,captured_revision,binary_bytes,decoder_version) VALUES(?1,?2,?3,?4,?5,?6,?7,zeroblob(?8),?9)", params![batch_id,ordinal,language,producer_id,sha256,source_set_id,captured_revision,length,descriptor.decoder_version])?;
            let capture_id = db.last_insert_rowid();
            fill_capture_blob(
                db,
                capture_id,
                &stage.artifact_path(u32::try_from(*ordinal)?),
                *length,
                &|| {
                    gate()?;
                    Ok(())
                },
            )?;
            capture_id
        };
        capture_ids.push((*ordinal, capture_id));
        gate()?;
    }
    let values = capture_ids
        .iter()
        .map(|(ordinal, capture_id)| format!("({ordinal},{capture_id})"))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(&with_capture_ids(&values, RAW_DOCUMENTS_SQL), [])?;
    gate()?;
    db.execute(RAW_EXCLUSIONS_SQL, [batch_id])?;
    gate()?;
    db.execute(&with_capture_ids(&values, RAW_PROOFS_SQL), [])?;
    Ok(batch_id)
}

/// Streams the staged `out_*` rows into the semantic tables one coverage tuple at a time:
/// each family is read by one cursor in emission order, so memory is bounded by one document
/// and the rows match what the in-memory writer produces.
fn write_staged_semantic(
    db: &Connection,
    key: &str,
    producer: &crate::semantic_evidence::Producer,
    gate: &dyn Fn() -> Result<()>,
) -> Result<()> {
    // The tuple writers' statements, each target table's included, stay prepared.
    db.set_prepared_statement_cache_capacity(64);
    let mut index = 0u64;
    crate::semantic_import::read_staged_documents(db, "stage.", producer, &mut |publication| {
        write_import_coverage(db, key, &publication)?;
        if index % 1000 == 999 {
            gate()?;
        }
        index += 1;
        Ok(())
    })
}

/// Witness fast path over a stored batch (R18). `Ok(None)` sends the rejoin to the FULL path.
fn attest_selected_batch_fast(
    raw: &SelectedRawBatch,
    producer: &crate::semantic_evidence::Producer,
    original: &crate::native_evidence::Artifact,
    inventory: &crate::scip_raw::ProofInventory,
) -> Result<Option<AttestedProfileProducer>> {
    use crate::scip_admission::{REFERENCE_TRUST_ORIGIN, pinned_producer_by_id, witness_language};
    use crate::semantic_batch::descriptor::{
        DescriptorProducer, FORMAT_VERSION, IdentityKind, SourceManifestHasher,
    };
    let descriptor = &raw.descriptor;
    let [capture] = raw.captures.as_slice() else {
        return Ok(None);
    };
    if descriptor.trust_origin != REFERENCE_TRUST_ORIGIN {
        return Ok(None);
    }
    let Some(language) = witness_language(&raw.producer_id) else {
        return Ok(None);
    };
    let Some(pinned) = pinned_producer_by_id(&raw.producer_id)
        .filter(|pinned| pinned.identity_kind == IdentityKind::Executable)
    else {
        return Ok(None);
    };
    let documents = &original.revision.documents;
    let source_manifest_hash = || {
        let mut hasher = SourceManifestHasher::new(&original.source_set.id);
        for doc in documents {
            hasher.push(&doc.key.language, &doc.key.path, &doc.content_hash);
        }
        hasher.finish()
    };
    let content_hashes: HashMap<(&str, &str), &str> = documents
        .iter()
        .map(|doc| {
            (
                (doc.key.language.as_str(), doc.key.path.as_str()),
                doc.content_hash.as_str(),
            )
        })
        .collect();
    if capture.binary_bytes.len() > 64 * 1024 * 1024
        || descriptor.format_version != FORMAT_VERSION
        || descriptor.decoder_version != crate::scip_raw::DECODER_VERSION
        || raw.decoder_version != crate::scip_raw::DECODER_VERSION
        || descriptor.profile_id != pinned.profile_id
        || descriptor.producer
            != (DescriptorProducer {
                name: pinned.name.clone(),
                version: pinned.version.clone(),
                identity_kind: IdentityKind::Executable,
                executable_hash: pinned.executable_hash.clone(),
                position_encoding: pinned.position_encoding,
                languages: vec![language.to_owned()],
            })
        || descriptor.source_set.id != original.source_set.id
        || descriptor.source_set.root_id != original.source_set.root_id
        || descriptor.source_set.languages != original.source_set.languages
        || descriptor.source_set.dependencies != original.source_set.dependencies
        || descriptor.source_revision != original.revision.id
        || descriptor.source_manifest_hash != raw.source_manifest_hash
        || descriptor.source_manifest_hash != inventory.source_manifest_hash
        || descriptor.source_manifest_hash != source_manifest_hash()
        || descriptor.totals.source_documents != documents.len() as u64
        || descriptor.components.toolchain_hash != original.revision.toolchain_hash
        || descriptor.components.config_hash != original.revision.config_hash
        || descriptor.components.dependency_hash != original.revision.dependency_hash
        || descriptor.artifacts.len() != 1
        || descriptor.artifacts[0].sha256 != capture.artifact_hash
        || descriptor.artifacts[0].language != language
        || descriptor.totals.covered_documents != raw.documents.len() as u64
        || raw
            .documents
            .iter()
            .any(|((doc_language, path), (source_hash, _, _))| {
                content_hashes.get(&(doc_language.as_str(), path.as_str()))
                    != Some(&source_hash.as_str())
            })
        || descriptor.totals.excluded_documents != raw.exclusions.len() as u64
    {
        return Ok(None);
    }
    let attested = AttestedProfileProducer {
        id: raw.producer_id.clone(),
        version: pinned.version.clone(),
        executable_hash: pinned.executable_hash.clone(),
        position_encoding: producer.position_encoding,
        languages: serde_json::from_value(serde_json::json!([language]))?,
    };
    Ok(attested.matches(producer).then_some(attested))
}

fn original_import_snapshot(
    tx: &Connection,
    raw: &SelectedRawBatch,
) -> Result<Option<crate::semantic_evidence::SemanticSnapshot>> {
    let key: Option<String> = tx
        .query_row(
            "SELECT s.revision_id FROM semantic_revision_raw_selections s
         JOIN native_revisions r ON r.id=s.revision_id
         WHERE s.batch_id=?1 AND s.producer_id=?2 AND s.captured_pin=?3
           AND r.native_revision_id=?4 AND r.source_set_id=?5
         ORDER BY r.published_index_revision LIMIT 1",
            params![
                raw.batch_id,
                raw.producer_id,
                semantic_pin_key(raw.captured_pin),
                raw.captured_revision,
                raw.source_set_id
            ],
            |row| row.get(0),
        )
        .optional()?;
    key.map(|key| read_semantic_tuple(tx, &key, None, None, true))
        .transpose()
}

fn original_document_snapshot(
    full: &crate::semantic_evidence::SemanticSnapshot,
    producer_id: &str,
    document: &crate::native_evidence::DocumentKey,
) -> crate::semantic_evidence::SemanticSnapshot {
    let ids: BTreeSet<_> = full
        .provenance
        .iter()
        .filter(|row| row.producer_id == producer_id && row.document == *document)
        .map(|row| row.id.as_str())
        .collect();
    crate::semantic_evidence::SemanticSnapshot {
        producers: full
            .producers
            .iter()
            .filter(|row| row.id == producer_id)
            .cloned()
            .collect(),
        coverage: full
            .coverage
            .iter()
            .filter(|row| {
                row.producer_id == producer_id
                    && row.source_set_id == document.source_set_id
                    && row.document_path == document.path
            })
            .cloned()
            .collect(),
        provenance: full
            .provenance
            .iter()
            .filter(|row| ids.contains(row.id.as_str()))
            .cloned()
            .collect(),
        symbols: full
            .symbols
            .iter()
            .filter(|row| ids.contains(row.provenance_id.as_str()))
            .cloned()
            .collect(),
        declaration_bindings: full
            .declaration_bindings
            .iter()
            .filter(|row| ids.contains(row.provenance_id.as_str()))
            .cloned()
            .collect(),
        references: full
            .references
            .iter()
            .filter(|row| ids.contains(row.provenance_id.as_str()))
            .cloned()
            .collect(),
        call_bindings: full
            .call_bindings
            .iter()
            .filter(|row| ids.contains(row.provenance_id.as_str()))
            .cloned()
            .collect(),
        type_relationships: full
            .type_relationships
            .iter()
            .filter(|row| ids.contains(row.provenance_id.as_str()))
            .cloned()
            .collect(),
    }
}

fn targeted_current_declaration(
    tx: &Connection,
    pin: IndexPin,
    document: &crate::native_evidence::DocumentKey,
    syntax_id: &str,
) -> Result<Option<crate::semantic_evidence::TargetDeclaration>> {
    #[cfg(feature = "test-support")]
    crate::semantic_import::note_current_target_lookup(document);
    let mut query = tx.prepare(
        "SELECT d.name,d.name_start,d.name_end FROM revision_documents m
         JOIN document_versions v ON v.vid=m.vid AND v.language=m.language AND v.path=m.path
         JOIN native_version_declarations d ON d.vid=v.vid
         WHERE m.revision_id=?1 AND m.language=?2 AND m.path=?3 AND d.syntax_id=?4
         LIMIT 2",
    )?;
    let mut rows = query.query(params![
        semantic_pin_key(pin),
        document.language,
        document.path,
        stored_sid(syntax_id)?
    ])?;
    let first = rows
        .next()?
        .map(|row| {
            let name: Option<String> = row.get(0)?;
            let start: Option<i64> = row.get(1)?;
            let end: Option<i64> = row.get(2)?;
            Ok::<_, rusqlite::Error>((name, start, end))
        })
        .transpose()?;
    ensure!(
        rows.next()?.is_none(),
        "incompatible_index: duplicate targeted current declaration"
    );
    #[cfg(feature = "test-support")]
    crate::semantic_import::note_current_target_rows(usize::from(first.is_some()));
    match first {
        Some((Some(name), Some(start), Some(end))) => {
            Ok(Some(crate::semantic_evidence::TargetDeclaration {
                syntax_id: syntax_id.to_owned(),
                document: document.clone(),
                name,
                name_range: crate::native_evidence::Range {
                    start: usize::try_from(start)?,
                    end: usize::try_from(end)?,
                },
            }))
        }
        Some((None, None, None)) | None => Ok(None),
        _ => Err(SelectedIntegrity(
            "incompatible_index: malformed targeted current declaration".into(),
        )
        .into()),
    }
}

fn native_version_at(
    db: &Connection,
    pin: IndexPin,
    doc: &crate::native_evidence::DocumentKey,
) -> Result<Option<String>> {
    db.query_row(
        "SELECT v.id FROM revision_documents m JOIN document_versions v ON v.vid=m.vid
         WHERE m.revision_id=?1 AND m.language=?2 AND m.path=?3 LIMIT 2",
        params![semantic_pin_key(pin), doc.language, doc.path],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn append_reconciled_document(
    db: &Connection,
    selected: &ReadRevision,
    doc: &crate::native_evidence::DocumentKey,
    next: &mut crate::native_evidence::Artifact,
    sources: &mut BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
) -> Result<()> {
    let scope = v8_native_scope_for(db, doc, selected)?
        .context("reused native document missing from selected pin")?;
    let file = Store::selected_source_row_bounded_for(db, &doc.path, 256 * 1024 * 1024, selected)?
        .context("reused native source missing from selected pin")?;
    selected_ensure!(
        file.language == doc.language,
        "incompatible_index: reused language differs"
    );
    sources.insert(doc.clone(), file.text.into_bytes());
    let mut coverage = read_native_coverage_v8(db, &scope)?;
    coverage.revision_id.clone_from(&next.revision.id);
    next.coverage.push(coverage);
    let mut declarations = read_native_declarations_v8(db, &scope, None, true)?;
    for row in &mut declarations {
        row.revision_id.clone_from(&next.revision.id);
    }
    next.declarations.extend(declarations);
    let mut calls = read_native_calls_v8(db, &scope, None)?;
    for row in &mut calls {
        row.revision_id.clone_from(&next.revision.id);
    }
    next.calls.extend(calls);
    let mut regions = read_native_control_regions_v8(db, &scope, None)?;
    for row in &mut regions {
        row.revision_id.clone_from(&next.revision.id);
    }
    next.control_regions.extend(regions);
    let mut references = read_native_references(db, &scope, None)?;
    for row in &mut references {
        row.revision_id.clone_from(&next.revision.id);
    }
    next.references.extend(references);
    let mut report = read_native_capability_report(db, &scope)?;
    report.revision_id.clone_from(&next.revision.id);
    next.capability_reports.push(report);
    Ok(())
}

fn append_captured_document(
    r1: &crate::native_evidence::Artifact,
    doc: &crate::native_evidence::DocumentKey,
    next: &mut crate::native_evidence::Artifact,
) {
    let revision = &next.revision.id;
    next.coverage.extend(
        r1.coverage
            .iter()
            .filter(|row| {
                row.document_path == doc.path
                    && row.language == doc.language
                    && row.source_set_id == doc.source_set_id
            })
            .cloned()
            .map(|mut row| {
                row.revision_id.clone_from(revision);
                row
            }),
    );
    next.declarations.extend(
        r1.declarations
            .iter()
            .filter(|row| row.document == *doc)
            .cloned()
            .map(|mut row| {
                row.revision_id.clone_from(revision);
                row
            }),
    );
    next.calls.extend(
        r1.calls
            .iter()
            .filter(|row| row.document == *doc)
            .cloned()
            .map(|mut row| {
                row.revision_id.clone_from(revision);
                row
            }),
    );
    next.control_regions.extend(
        r1.control_regions
            .iter()
            .filter(|row| row.document == *doc)
            .cloned()
            .map(|mut row| {
                row.revision_id.clone_from(revision);
                row
            }),
    );
    next.references.extend(
        r1.references
            .iter()
            .filter(|row| row.document == *doc)
            .cloned()
            .map(|mut row| {
                row.revision_id.clone_from(revision);
                row
            }),
    );
    next.capability_reports.extend(
        r1.capability_reports
            .iter()
            .filter(|row| row.document == *doc)
            .cloned()
            .map(|mut row| {
                row.revision_id.clone_from(revision);
                row
            }),
    );
}

type IncrementalDestination = (
    crate::native_evidence::Artifact,
    BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    Vec<BTreeSet<crate::native_evidence::DocumentKey>>,
);

fn witnessed_native_destination(
    tx: &Connection,
    selected: &[SelectedNativeInput],
    native: &crate::native_evidence::Artifact,
    sources: &BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    reuse: &PreflightReuse,
    prior: IndexPin,
) -> Result<Option<IncrementalDestination>> {
    let mut reusable = BTreeSet::new();
    for old in selected {
        let (Ok(raw), Ok((captured, _, _))) = (&old.raw, &old.captured) else {
            return Ok(None);
        };
        if raw.producer_id != old.producer.id
            || native.producer != captured.producer
            || native.revision.toolchain_hash != captured.revision.toolchain_hash
            || native.revision.dependency_hash != captured.revision.dependency_hash
        {
            return Ok(None);
        }
        for doc in &captured.revision.documents {
            let Some(current) = native
                .revision
                .documents
                .iter()
                .find(|row| row.key == doc.key)
            else {
                continue;
            };
            if current.content_hash != doc.content_hash {
                continue;
            }
            // Captured bytes may have been restored after the prior publication.
            // Without version reuse, FULL must measure their original associations.
            if !reuse.for_path(&doc.key.path).native {
                return Err(WitnessFallback("restored_document_requires_measurement").into());
            }
            let Some(captured_version) = native_version_at(tx, raw.captured_pin, &doc.key)? else {
                return Ok(None);
            };
            let Some(current_version) = native_version_at(tx, prior, &doc.key)? else {
                return Ok(None);
            };
            if captured_version != current_version {
                return Ok(None);
            };
            reusable.insert(doc.key.clone());
        }
    }
    if reusable.is_empty() {
        return Ok(None);
    }
    let selected_sources = BTreeMap::from_iter(
        sources
            .iter()
            .map(|(doc, bytes)| (doc.clone(), bytes.clone())),
    );
    let reusable_per_producer = selected.iter().map(|_| reusable.clone()).collect();
    Ok(Some((
        native.clone(),
        selected_sources,
        reusable_per_producer,
    )))
}

fn incremental_native_destination(
    tx: &Connection,
    selected: &[SelectedNativeInput],
    native: &crate::native_evidence::Artifact,
    sources: &BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    reuse: &PreflightReuse,
    prior: IndexPin,
) -> Result<Option<IncrementalDestination>> {
    let old = ReadRevision::new(prior, semantic_pin_key(prior));
    let mut candidate = native.clone();
    let mut selected_sources = BTreeMap::new();
    for doc in &native.revision.documents {
        if !reuse.for_path(&doc.key.path).native {
            selected_sources.insert(
                doc.key.clone(),
                sources
                    .get(&doc.key)
                    .context("changed native source missing")?
                    .clone(),
            );
        }
    }
    let mut seen = BTreeSet::new();
    let mut reused_by_producer = Vec::new();
    let documents: BTreeMap<_, _> = native
        .revision
        .documents
        .iter()
        .map(|doc| (&doc.key, doc))
        .collect();
    for input in selected {
        if input.raw.is_err() || input.captured.is_err() {
            return Ok(None);
        }
        let reusable = BTreeSet::new();
        if let (Ok(raw), Ok((r1, r1_sources, _))) = (&input.raw, &input.captured) {
            if raw.producer_id != input.producer.id
                || raw.proofs.iter().any(|(_, proof)| {
                    let key = (proof.language.clone(), proof.path.clone());
                    let expected = r1.revision.documents.iter().find(|doc| {
                        doc.key.language == proof.language && doc.key.path == proof.path
                    });
                    !matches!((raw.documents.get(&key), expected),
                        (Some((source_hash, wire_digest, _)), Some(doc))
                        if source_hash == &doc.content_hash
                            && wire_digest.len() == 64
                            && wire_digest.bytes().all(|b| b.is_ascii_hexdigit()))
                })
                || native.producer != r1.producer
                || native.revision.toolchain_hash != r1.revision.toolchain_hash
                || native.revision.config_hash != r1.revision.config_hash
                || native.revision.dependency_hash != r1.revision.dependency_hash
            {
                return Ok(None);
            }
            for doc in &r1.revision.documents {
                let Some(current) = documents.get(&doc.key) else {
                    continue;
                };
                if !seen.insert(doc.key.clone()) {
                    continue;
                }
                let new_version = reuse.for_path(&doc.key.path).native;
                if !new_version {
                    selected_sources.insert(
                        doc.key.clone(),
                        sources
                            .get(&doc.key)
                            .context("changed native source missing")?
                            .clone(),
                    );
                    continue;
                }
                let current_version = native_version_at(tx, prior, &doc.key)?
                    .context("selected native version missing")?;
                let captured_version = native_version_at(tx, raw.captured_pin, &doc.key)?
                    .context("captured native version missing")?;
                if current.content_hash == doc.content_hash
                    && current_version == captured_version
                    && native.producer == r1.producer
                    && raw
                        .documents
                        .get(&(doc.key.language.clone(), doc.key.path.clone()))
                        .is_none_or(|(source_hash, wire_digest, _)| {
                            source_hash == &doc.content_hash
                                && wire_digest.len() == 64
                                && wire_digest.bytes().all(|b| b.is_ascii_hexdigit())
                        })
                {
                    append_captured_document(r1, &doc.key, &mut candidate);
                    selected_sources.insert(
                        doc.key.clone(),
                        r1_sources
                            .get(&doc.key)
                            .context("captured native source missing")?
                            .clone(),
                    );
                } else if current.content_hash == doc.content_hash {
                    return Ok(None);
                } else {
                    append_reconciled_document(
                        tx,
                        &old,
                        &doc.key,
                        &mut candidate,
                        &mut selected_sources,
                    )?;
                }
            }
        }
        reused_by_producer.push(reusable);
    }
    // A later selected producer can share the same source document. Its witness
    // must be checked independently even when another producer supplied the rows.
    for (input, reusable) in selected.iter().zip(&mut reused_by_producer) {
        if let (Ok(raw), Ok((r1, _, _))) = (&input.raw, &input.captured) {
            for doc in &r1.revision.documents {
                let Some(current) = documents.get(&doc.key) else {
                    continue;
                };
                if current.content_hash != doc.content_hash || !reuse.for_path(&doc.key.path).native
                {
                    continue;
                }
                let current_version = native_version_at(tx, prior, &doc.key)?
                    .context("selected native version missing")?;
                let captured_version = native_version_at(tx, raw.captured_pin, &doc.key)?
                    .context("captured native version missing")?;
                if current_version == captured_version && native.producer == r1.producer {
                    reusable.insert(doc.key.clone());
                } else {
                    return Ok(None);
                }
            }
        }
    }
    candidate
        .revision
        .documents
        .retain(|doc| selected_sources.contains_key(&doc.key));
    Ok(Some((candidate, selected_sources, reused_by_producer)))
}

fn complete_local_destination(
    tx: &Connection,
    native: &crate::native_evidence::Artifact,
    sources: &BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    reuse: &PreflightReuse,
    prior: IndexPin,
) -> Result<(
    crate::native_evidence::Artifact,
    BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
)> {
    let old = ReadRevision::new(prior, semantic_pin_key(prior));
    let mut candidate = native.clone();
    let mut selected_sources = sources.clone();
    for doc in &native.revision.documents {
        if reuse.for_path(&doc.key.path).native {
            append_reconciled_document(tx, &old, &doc.key, &mut candidate, &mut selected_sources)?;
        }
    }
    Ok((candidate, selected_sources))
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn try_original_witness_rejoin(
    tx: &Connection,
    raw: &SelectedRawBatch,
    producer: &crate::semantic_evidence::Producer,
    inventory: &crate::scip_raw::ProofInventory,
    original_native: &crate::native_evidence::Artifact,
    original_sources: &BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    destination: &crate::native_evidence::Artifact,
    destination_sources: &BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    reusable: &BTreeSet<crate::native_evidence::DocumentKey>,
    prior: IndexPin,
    pin: IndexPin,
    toolchain: &serde_json::Value,
    config: &[serde_json::Value],
    dependencies: &[serde_json::Value],
    shared_changed_index: &crate::semantic_import::MeasuredFacts<'_>,
) -> Result<
    Option<(
        crate::semantic_import::RejoinResult,
        crate::semantic_evidence::CapturedComponents,
        crate::store::WitnessedRawCapture,
        AttestedProfileProducer,
        BTreeMap<crate::native_evidence::DocumentKey, crate::semantic_evidence::WitnessedDocument>,
        Vec<crate::semantic_evidence::TargetDeclaration>,
        crate::native_evidence::Artifact,
    )>,
> {
    use crate::{native_ids, semantic_evidence as sem};
    if reusable.is_empty()
        || destination.producer != original_native.producer
        || destination.revision.toolchain_hash != original_native.revision.toolchain_hash
        || destination.revision.dependency_hash != original_native.revision.dependency_hash
        || raw.captured_revision != original_native.revision.id
    {
        return Ok(None);
    }
    let Some(original) = original_import_snapshot(tx, raw)? else {
        return Ok(None);
    };
    let Some(attested) = attest_selected_batch_fast(raw, producer, original_native, inventory)?
    else {
        return Ok(None);
    };
    let capture = &raw.captures[0];
    let mut witnesses = BTreeMap::new();
    for doc in reusable {
        let Some(old) = original_native
            .revision
            .documents
            .iter()
            .find(|row| &row.key == doc)
        else {
            return Ok(None);
        };
        let Some(current) = destination
            .revision
            .documents
            .iter()
            .find(|row| &row.key == doc)
        else {
            return Ok(None);
        };
        let covered = producer.languages.iter().any(|language| {
            serde_json::to_value(language)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .as_deref()
                == Some(doc.language.as_str())
        });
        let admitted = raw.documents.get(&(doc.language.clone(), doc.path.clone()));
        if covered != admitted.is_some() {
            return Ok(None);
        }
        let (source_hash, wire_digest) = admitted
            .map(|(hash, digest, _)| (hash.as_str(), Some(digest.clone())))
            .unwrap_or((&old.content_hash, None));
        let Some(captured_version) = native_version_at(tx, raw.captured_pin, doc)? else {
            return Ok(None);
        };
        let Some(current_version) = native_version_at(tx, prior, doc)? else {
            return Ok(None);
        };
        if captured_version != current_version
            || old.content_hash != source_hash
            || current.content_hash != old.content_hash
            || wire_digest.as_ref().is_some_and(|digest| {
                digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            return Ok(None);
        }
        let mut selected = original_document_snapshot(&original, &raw.producer_id, doc);
        if !covered {
            selected.producers.clear();
        }
        if (covered
            && (selected.coverage.len() != 1
                || !selected.coverage[0].requested
                || !selected.coverage[0].selected))
            || (!covered
                && (!selected.coverage.is_empty()
                    || !selected.provenance.is_empty()
                    || inventory
                        .claims
                        .iter()
                        .any(|proof| proof.language == doc.language && proof.path == doc.path)))
            || selected.provenance.iter().any(|row| {
                row.revision_id != raw.captured_revision
                    || row.content_hash != source_hash
                    || row.basis.as_ref().is_none_or(|basis| {
                        basis.revision_id != raw.captured_revision
                            || basis.artifact_hash != capture.artifact_hash
                            || basis.producer_id != raw.producer_id
                            || basis.source_set_id != raw.source_set_id
                    })
            })
        {
            return Ok(None);
        }
        witnesses.insert(
            doc.clone(),
            sem::WitnessedDocument {
                source_hash: source_hash.to_owned(),
                wire_digest,
                captured_version,
                current_version,
                original: selected,
            },
        );
    }
    let mut basis = BTreeMap::new();
    for doc in &original_native.revision.documents {
        let language: sem::Language = serde_json::from_value(serde_json::json!(doc.key.language))?;
        if producer.languages.contains(&language) {
            basis.insert(
                doc.key.clone(),
                sem::SemanticBasis {
                    producer_id: producer.id.clone(),
                    producer_version: producer.version.clone(),
                    producer_hash: producer.executable_hash.clone(),
                    artifact_hash: capture.artifact_hash.clone(),
                    language,
                    source_set_id: doc.key.source_set_id.clone(),
                    revision_id: original_native.revision.id.clone(),
                    source_manifest_hash: inventory.source_manifest_hash.clone(),
                    toolchain_hash: original_native.revision.toolchain_hash.clone(),
                    config_hash: original_native.revision.config_hash.clone(),
                    dependency_hash: original_native.revision.dependency_hash.clone(),
                    lookup_dependencies: vec![],
                },
            );
        }
    }
    let Some(target_ids) =
        crate::semantic_import::required_original_targets(&original, inventory, &producer.id)
    else {
        return Ok(None);
    };
    let mut targets = BTreeMap::new();
    for (document, id) in target_ids {
        if let Some(target) = targeted_current_declaration(tx, pin, &document, &id)? {
            targets.insert((document, id), target);
        }
    }
    let target_rows = targets.values().cloned().collect();
    let changed_sources: BTreeMap<_, _> = destination_sources
        .iter()
        .filter(|(doc, _)| !reusable.contains(*doc))
        .map(|(doc, bytes)| (doc.clone(), bytes.clone()))
        .collect();
    let mut measured = destination.clone();
    measured.coverage.retain(|row| {
        changed_sources.keys().any(|doc| {
            doc.source_set_id == row.source_set_id
                && doc.language == row.language
                && doc.path == row.document_path
        })
    });
    measured
        .provenance
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .declarations
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .references
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .calls
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .control_regions
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .capability_reports
        .retain(|row| changed_sources.contains_key(&row.document));
    let outcome = crate::semantic_import::rejoin_inventory_with_original_witness(
        inventory,
        original_native,
        &measured,
        producer.clone(),
        &basis,
        &original,
        reusable,
        &targets,
        shared_changed_index,
    )?;
    let Some(outcome) = outcome else {
        return Ok(None);
    };
    let captured = sem::CapturedComponents {
        producer_executables: BTreeMap::new(),
        artifacts: BTreeMap::from([(
            (raw.producer_id.clone(), capture.artifact_hash.clone()),
            capture.binary_bytes.clone(),
        )]),
        sources: original_sources
            .iter()
            .filter(|(doc, _)| !reusable.contains(*doc))
            .map(|(doc, bytes)| (doc.clone(), bytes.clone()))
            .collect(),
        captured_revision: original_native.revision.clone(),
        native_toolchain: toolchain.clone(),
        native_config: config.to_vec(),
        native_dependencies: dependencies.to_vec(),
        requested_producers: vec![producer.clone()],
        toolchain: native_ids::canonical(toolchain),
        config: native_ids::canonical(&serde_json::json!(config)),
        dependency: native_ids::canonical(&serde_json::json!(dependencies)),
        native_extraction_components: BTreeMap::new(),
    };
    let raw_capture = WitnessedRawCapture {
        producer_id: raw.producer_id.clone(),
        source_set_id: raw.source_set_id.clone(),
        captured_revision: raw.captured_revision.clone(),
        binary_bytes: capture.binary_bytes.clone(),
        inventory: inventory.clone(),
    };
    Ok(Some((
        outcome,
        captured,
        raw_capture,
        attested,
        witnesses,
        target_rows,
        measured,
    )))
}

#[derive(Debug)]
struct WitnessFallback(&'static str);
impl std::fmt::Display for WitnessFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "selected original association requires FULL rejoin: {}",
            self.0
        )
    }
}
impl std::error::Error for WitnessFallback {}

#[cfg(feature = "test-support")]
#[derive(Debug)]
struct RejoinOracleMismatch;
#[cfg(feature = "test-support")]
impl std::fmt::Display for RejoinOracleMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LOCAL publication differs from independent FULL rejoin oracle")
    }
}
#[cfg(feature = "test-support")]
impl std::error::Error for RejoinOracleMismatch {}
#[cfg(feature = "test-support")]
thread_local! {
    static TEST_REJOIN_ORACLE_MISMATCH: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn rejoin_storage_failure(error: &anyhow::Error) -> bool {
    error.is::<rusqlite::Error>()
        || error.is::<std::io::Error>()
        || error.is::<SelectedIntegrity>()
        || error.is::<ControlIntegrity>()
        || error.is::<SqliteContention>()
        || error.is::<HotIndexJournal>()
}

fn rejoin_storage_error(error: anyhow::Error) -> anyhow::Error {
    match error.downcast::<rusqlite::Error>() {
        Ok(sqlite) => storage_result::<()>(Err(sqlite)).expect_err("SQLite error remains an error"),
        Err(error) => error,
    }
}

fn record_rejoin_fallback(reason: &'static str) {
    tracing::debug!(reason, "selected semantic rejoin uses FULL fallback");
    #[cfg(feature = "test-support")]
    crate::semantic_import::note_fallback_reason(reason);
}

fn decoded_selected_inventory(
    raw: &SelectedRawBatch,
) -> Result<crate::scip_raw::CapturedScipInventory> {
    Ok(crate::scip_raw::CapturedScipInventory {
        artifact_hashes: raw
            .captures
            .iter()
            .map(|capture| capture.artifact_hash.clone())
            .collect(),
        decoder_version: raw.decoder_version.clone(),
        source_manifest_hash: raw.source_manifest_hash.clone(),
        completion: raw.descriptor.completion,
        exclusions: raw.exclusions.clone(),
        capped_documents: raw.capped_documents.clone(),
        claims: raw
            .proofs
            .iter()
            .map(|(id, proof)| {
                Ok(crate::scip_raw::CapturedClaim {
                    envelope: {
                        #[cfg(feature = "test-support")]
                        crate::semantic_import::note_original_raw_envelope_parse();
                        let envelope: serde_json::Value =
                            serde_json::from_slice(&proof.canonical_bytes)?;
                        crate::scip_raw::CapturedScipFactV1 {
                            format_version: u8::try_from(
                                envelope["formatVersion"]
                                    .as_u64()
                                    .context("raw format version")?,
                            )?,
                            record_kind: envelope["recordKind"]
                                .as_str()
                                .context("raw record kind")?
                                .to_owned(),
                            relative_path: envelope["relativePath"]
                                .as_str()
                                .context("raw path")?
                                .to_owned(),
                            position_encoding: u8::try_from(
                                envelope["positionEncoding"]
                                    .as_u64()
                                    .context("raw position encoding")?,
                            )?,
                            raw: envelope["raw"].clone(),
                            cargo_target: crate::scip_raw::CapturedScipFactV1::cargo_target_from(
                                &envelope,
                            )?,
                        }
                    },
                    canonical_bytes: proof.canonical_bytes.clone(),
                    fact_digest: proof.fact_digest.clone(),
                    captured_proof_id: id.clone(),
                    language: proof.language.clone(),
                    artifact_hash: proof.artifact_hash.clone(),
                })
            })
            .collect::<Result<_>>()?,
    })
}

#[allow(clippy::too_many_arguments)]
fn selected_native_rejoin(
    store: &Store,
    tx: &Connection,
    selected: &[SelectedNativeInput],
    native: &crate::native_evidence::Artifact,
    sources: &BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    pin: IndexPin,
    cancel: &CancelFlag,
    local_reuse: Option<&PreflightReuse>,
    prior_pin: Option<IndexPin>,
) -> Result<()> {
    if selected.iter().all(|input| input.lightweight) && !selected.is_empty() {
        tx.execute_batch("SAVEPOINT original_witness_rejoin")?;
        let attempt = selected_native_rejoin_once(
            store,
            tx,
            selected,
            native,
            sources,
            pin,
            cancel,
            local_reuse,
            prior_pin,
            false,
        );
        match attempt {
            Ok(()) => {
                tx.execute_batch("RELEASE original_witness_rejoin")?;
                return Ok(());
            }
            Err(error) => {
                tx.execute_batch(
                    "ROLLBACK TO original_witness_rejoin; RELEASE original_witness_rejoin",
                )?;
                let Some(fallback) = error.downcast_ref::<WitnessFallback>() else {
                    return Err(error);
                };
                #[cfg(feature = "test-support")]
                crate::semantic_import::clear_witnessed_rejoins();
                record_rejoin_fallback(fallback.0);
                let prior = prior_pin.context("missing selected prior pin for FULL fallback")?;
                let read = ReadRevision::new(prior, semantic_pin_key(prior));
                let full = store.selected_native_inputs_from_db(tx, &read, prior, None)?;
                #[cfg(feature = "test-support")]
                crate::semantic_import::note_full_fallback();
                return selected_native_rejoin_once(
                    store,
                    tx,
                    &full,
                    native,
                    sources,
                    pin,
                    cancel,
                    local_reuse,
                    prior_pin,
                    true,
                );
            }
        }
    }
    selected_native_rejoin_once(
        store,
        tx,
        selected,
        native,
        sources,
        pin,
        cancel,
        local_reuse,
        prior_pin,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn selected_native_rejoin_once(
    store: &Store,
    tx: &Connection,
    selected: &[SelectedNativeInput],
    native: &crate::native_evidence::Artifact,
    sources: &BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    pin: IndexPin,
    cancel: &CancelFlag,
    local_reuse: Option<&PreflightReuse>,
    prior_pin: Option<IndexPin>,
    force_full_after_witness: bool,
) -> Result<()> {
    use crate::{native_ids, semantic_evidence as sem};
    let key = semantic_pin_key(pin);
    // The destination is shared by every selected producer. A corrupt measured
    // candidate must abort this publication, never become one producer's failure.
    #[cfg(feature = "test-support")]
    let force_full = std::env::var_os("TRELLIS_TEST_FORCE_FULL_REJOIN").is_some();
    #[cfg(not(feature = "test-support"))]
    let force_full = false;
    let incremental = if let (Some(reuse), Some(prior)) = (local_reuse, prior_pin)
        && !force_full
        && !force_full_after_witness
    {
        if selected.iter().all(|input| input.lightweight) {
            witnessed_native_destination(tx, selected, native, sources, reuse, prior)?
        } else {
            incremental_native_destination(tx, selected, native, sources, reuse, prior)?
        }
    } else {
        None
    };
    if incremental.is_none() && selected.iter().any(|input| input.lightweight) {
        return Err(WitnessFallback("destination_witness_unavailable").into());
    }
    let used_full_fallback = incremental.is_none()
        && local_reuse.is_some()
        && prior_pin.is_some()
        && !force_full
        && !force_full_after_witness;
    if used_full_fallback {
        record_rejoin_fallback("incremental_destination_unavailable");
    }
    let (candidate, selected_sources, reusable) = match incremental {
        Some((candidate, sources, reusable)) => (candidate, sources, Some(reusable)),
        None if let (Some(reuse), Some(prior)) = (local_reuse, prior_pin) => {
            let (candidate, sources) =
                complete_local_destination(tx, native, sources, reuse, prior)?;
            (candidate, sources, None)
        }
        None => (native.clone(), sources.clone(), None),
    };
    #[cfg(feature = "test-support")]
    if std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE").is_some() {
        let reused = reusable
            .as_ref()
            .into_iter()
            .flatten()
            .flat_map(|docs| docs.iter().cloned())
            .collect();
        crate::semantic_import::set_publication_reused(reused);
        if used_full_fallback {
            crate::semantic_import::note_full_fallback();
        }
    }
    #[cfg(feature = "test-support")]
    let full_oracle = if (std::env::var_os("TRELLIS_TEST_LOCAL_FULL_ORACLE").is_some()
        || TEST_REJOIN_ORACLE_MISMATCH.get())
        && let (Some(reuse), Some(prior)) = (local_reuse, prior_pin)
        && reusable
            .as_ref()
            .is_some_and(|per_producer| per_producer.iter().any(|docs| !docs.is_empty()))
    {
        Some(complete_local_destination(
            tx, native, sources, reuse, prior,
        )?)
    } else {
        None
    };
    let mut legacy_destination = None;
    let fast_reusable = reusable
        .as_ref()
        .and_then(|items| items.first())
        .filter(|first| {
            !first.is_empty()
                && reusable
                    .as_ref()
                    .is_some_and(|sets| sets.iter().all(|set| set == *first))
        });
    let fast_changed_sources: BTreeMap<_, _> = fast_reusable
        .into_iter()
        .flat_map(|reused| {
            selected_sources
                .iter()
                .filter(move |(doc, _)| !reused.contains(*doc))
                .map(|(doc, bytes)| (doc.clone(), bytes.clone()))
        })
        .collect();
    let mut fast_delta = candidate.clone();
    fast_delta
        .revision
        .documents
        .retain(|doc| fast_changed_sources.contains_key(&doc.key));
    fast_delta.coverage.retain(|row| {
        fast_changed_sources.keys().any(|doc| {
            doc.source_set_id == row.source_set_id
                && doc.language == row.language
                && doc.path == row.document_path
        })
    });
    fast_delta
        .provenance
        .retain(|row| fast_changed_sources.contains_key(&row.document));
    fast_delta
        .declarations
        .retain(|row| fast_changed_sources.contains_key(&row.document));
    fast_delta
        .references
        .retain(|row| fast_changed_sources.contains_key(&row.document));
    fast_delta
        .calls
        .retain(|row| fast_changed_sources.contains_key(&row.document));
    fast_delta
        .control_regions
        .retain(|row| fast_changed_sources.contains_key(&row.document));
    fast_delta
        .capability_reports
        .retain(|row| fast_changed_sources.contains_key(&row.document));
    let fast_index = if fast_reusable.is_some() && selected.iter().all(|input| input.lightweight) {
        Some(crate::semantic_import::MeasuredFacts::new(
            &fast_delta,
            &fast_changed_sources,
        )?)
    } else {
        None
    };
    for (index, input) in selected.iter().enumerate() {
        check_cancel(cancel)?;
        let attempt = (|| -> Result<(sem::SemanticPublication, sem::CapturedComponents)> {
            let raw = input
                .raw
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error:#}"))?;
            let (r1, r1_sources, (toolchain, config, dependencies)) = input
                .captured
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error:#}"))?;
            let metadata_inventory = crate::scip_raw::ProofInventory {
                artifact_hashes: raw
                    .captures
                    .iter()
                    .map(|capture| capture.artifact_hash.clone())
                    .collect(),
                source_manifest_hash: raw.source_manifest_hash.clone(),
                exclusions: raw.exclusions.clone(),
                capped_documents: raw.capped_documents.clone(),
                claims: input.proof_identities.clone(),
            };
            if input.lightweight {
                if raw.descriptor.trust_origin == crate::scip_admission::SYNTHETIC_TRUST_ORIGIN {
                    return Err(WitnessFallback("synthetic_producer").into());
                }
                if raw.captures.len() > 1 {
                    return Err(WitnessFallback("multi_artifact_batch").into());
                }
                let (Some(prior), Some(per_producer), Some(indexed)) =
                    (prior_pin, reusable.as_ref(), fast_index.as_ref())
                else {
                    return Err(WitnessFallback("shared_witness_index_unavailable").into());
                };
                let (outcome, captured, raw_capture, attested, witnesses, targets, measured) =
                    match try_original_witness_rejoin(
                        tx,
                        raw,
                        &input.producer,
                        &metadata_inventory,
                        r1,
                        r1_sources,
                        &candidate,
                        &selected_sources,
                        &per_producer[index],
                        prior,
                        pin,
                        toolchain,
                        config,
                        dependencies,
                        indexed,
                    ) {
                        Ok(Some(witness)) => witness,
                        Ok(None) => {
                            return Err(WitnessFallback("source_witness_unavailable").into());
                        }
                        Err(error) => return Err(error),
                    };
                let witness_validation = sem::validate_evidence_with_witnessed_raw(
                    &outcome.publication,
                    &measured,
                    &captured,
                    &raw_capture,
                    &attested,
                    &witnesses,
                    &targets,
                );
                if witness_validation.is_err() {
                    return Err(WitnessFallback("witness_validation_failed").into());
                }
                #[cfg(feature = "test-support")]
                crate::semantic_import::note_witnessed_rejoin();
                #[cfg(feature = "test-support")]
                if let Some(file) = std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE") {
                    let mut work = crate::semantic_import::publication_work();
                    work["indexPin"] = serde_json::to_value(pin)?;
                    work["producerIds"] = serde_json::to_value(
                        selected
                            .iter()
                            .map(|input| input.producer.id.clone())
                            .collect::<Vec<_>>(),
                    )?;
                    std::fs::write(file, serde_json::to_vec(&work)?)?;
                }
                #[cfg(feature = "test-support")]
                if let Some((oracle_native, oracle_sources)) = &full_oracle {
                    let work_checkpoint = crate::semantic_import::publication_work_checkpoint();
                    let selected_pin = prior_pin.context("missing prior pin for FULL oracle")?;
                    let selected_read =
                        ReadRevision::new(selected_pin, semantic_pin_key(selected_pin));
                    let full_selected = store.selected_native_inputs_from_db(
                        tx,
                        &selected_read,
                        selected_pin,
                        None,
                    )?;
                    let full_input = full_selected
                        .iter()
                        .find(|row| row.producer.id == input.producer.id)
                        .context("FULL oracle selected producer missing")?;
                    let full_raw = full_input
                        .raw
                        .as_ref()
                        .map_err(|error| anyhow::anyhow!("{error:#}"))?;
                    let (full_r1, full_sources, (full_toolchain, full_config, full_dependencies)) =
                        full_input
                            .captured
                            .as_ref()
                            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
                    let full_inventory = decoded_selected_inventory(full_raw)?;
                    let full_captured = sem::CapturedComponents {
                        producer_executables: BTreeMap::new(),
                        artifacts: captured_artifacts(full_raw),
                        sources: full_sources.clone(),
                        captured_revision: full_r1.revision.clone(),
                        native_toolchain: full_toolchain.clone(),
                        native_config: full_config.clone(),
                        native_dependencies: full_dependencies.clone(),
                        requested_producers: vec![input.producer.clone()],
                        toolchain: native_ids::canonical(full_toolchain),
                        config: native_ids::canonical(&serde_json::json!(full_config)),
                        dependency: native_ids::canonical(&serde_json::json!(full_dependencies)),
                        native_extraction_components: BTreeMap::new(),
                    };
                    let full_attested = authenticate_stored_batch(
                        full_raw,
                        &full_inventory,
                        &full_captured,
                        full_r1,
                        full_sources,
                        &store.identity.root,
                        cancel,
                    )?;
                    ensure!(
                        full_attested.matches(&input.producer),
                        "FULL oracle producer descriptor mismatch"
                    );
                    let mut full_basis = BTreeMap::new();
                    for doc in &full_r1.revision.documents {
                        let language: sem::Language =
                            serde_json::from_value(serde_json::json!(doc.key.language))?;
                        if input.producer.languages.contains(&language) {
                            full_basis.insert(
                                doc.key.clone(),
                                sem::SemanticBasis {
                                    producer_id: input.producer.id.clone(),
                                    producer_version: input.producer.version.clone(),
                                    producer_hash: input.producer.executable_hash.clone(),
                                    artifact_hash: full_raw.artifact_hash_for(&doc.key).to_owned(),
                                    language,
                                    source_set_id: doc.key.source_set_id.clone(),
                                    revision_id: full_r1.revision.id.clone(),
                                    source_manifest_hash: full_inventory
                                        .source_manifest_hash
                                        .clone(),
                                    toolchain_hash: full_r1.revision.toolchain_hash.clone(),
                                    config_hash: full_r1.revision.config_hash.clone(),
                                    dependency_hash: full_r1.revision.dependency_hash.clone(),
                                    lookup_dependencies: vec![],
                                },
                            );
                        }
                    }
                    let full = crate::semantic_import::rejoin_inventory(
                        &full_inventory,
                        full_r1,
                        full_sources,
                        oracle_native,
                        oracle_sources,
                        input.producer.clone(),
                        &full_basis,
                        match input.producer.position_encoding {
                            sem::PositionEncoding::Utf8 => 1,
                            sem::PositionEncoding::Utf16 => 2,
                            sem::PositionEncoding::UnicodeScalar => 3,
                        },
                    )?;
                    if TEST_REJOIN_ORACLE_MISMATCH.get()
                        || !(serde_json::to_vec(&outcome.publication)?
                            == serde_json::to_vec(&full.publication)?
                            && serde_json::to_vec(&outcome.report)?
                                == serde_json::to_vec(&full.report)?)
                    {
                        return Err(RejoinOracleMismatch.into());
                    }
                    crate::semantic_import::restore_publication_work(work_checkpoint);
                }
                return Ok((outcome.publication, captured));
            }
            let inventory = decoded_selected_inventory(raw)?;
            if legacy_destination.is_none() {
                legacy_destination = Some(crate::semantic_import::MeasuredFacts::new(
                    &candidate,
                    &selected_sources,
                )?);
            }
            let destination = legacy_destination
                .as_ref()
                .context("missing shared native destination")?;
            let captured = sem::CapturedComponents {
                producer_executables: BTreeMap::new(),
                artifacts: captured_artifacts(raw),
                sources: r1_sources.clone(),
                captured_revision: r1.revision.clone(),
                native_toolchain: toolchain.clone(),
                native_config: config.clone(),
                native_dependencies: dependencies.clone(),
                requested_producers: vec![input.producer.clone()],
                toolchain: native_ids::canonical(toolchain),
                config: native_ids::canonical(&serde_json::json!(config)),
                dependency: native_ids::canonical(&serde_json::json!(dependencies)),
                native_extraction_components: BTreeMap::new(),
            };
            let attested = authenticate_stored_batch(
                raw,
                &inventory,
                &captured,
                r1,
                r1_sources,
                &store.identity.root,
                cancel,
            )?;
            ensure!(
                attested.matches(&input.producer),
                "selected producer descriptor differs from authenticated profile"
            );
            let encoding = match input.producer.position_encoding {
                sem::PositionEncoding::Utf8 => 1,
                sem::PositionEncoding::Utf16 => 2,
                sem::PositionEncoding::UnicodeScalar => 3,
            };
            let mut basis = BTreeMap::new();
            for doc in &r1.revision.documents {
                let language: sem::Language =
                    serde_json::from_value(serde_json::json!(doc.key.language))?;
                if input.producer.languages.contains(&language) {
                    basis.insert(
                        doc.key.clone(),
                        sem::SemanticBasis {
                            producer_id: input.producer.id.clone(),
                            producer_version: input.producer.version.clone(),
                            producer_hash: input.producer.executable_hash.clone(),
                            artifact_hash: raw.artifact_hash_for(&doc.key).to_owned(),
                            language,
                            source_set_id: doc.key.source_set_id.clone(),
                            revision_id: r1.revision.id.clone(),
                            source_manifest_hash: inventory.source_manifest_hash.clone(),
                            toolchain_hash: r1.revision.toolchain_hash.clone(),
                            config_hash: r1.revision.config_hash.clone(),
                            dependency_hash: r1.revision.dependency_hash.clone(),
                            lookup_dependencies: vec![],
                        },
                    );
                }
            }
            let mut validated_native = candidate.clone();
            validated_native
                .revision
                .documents
                .clone_from(&native.revision.documents);
            let outcome = crate::semantic_import::rejoin_inventory_with_reuse(
                &inventory,
                r1,
                r1_sources,
                &validated_native,
                &selected_sources,
                input.producer.clone(),
                &basis,
                encoding,
                destination,
                reusable.as_ref().map(|per_producer| &per_producer[index]),
            )?;
            #[cfg(feature = "test-support")]
            if let Some(file) = std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE") {
                std::fs::write(file, {
                    let mut work = crate::semantic_import::publication_work();
                    work["indexPin"] = serde_json::to_value(pin)?;
                    work["producerIds"] = serde_json::to_value(
                        selected
                            .iter()
                            .map(|input| input.producer.id.clone())
                            .collect::<Vec<_>>(),
                    )?;
                    serde_json::to_vec(&work)?
                })?;
            }
            #[cfg(feature = "test-support")]
            if let Some((oracle_native, oracle_sources)) = &full_oracle {
                let full = crate::semantic_import::rejoin_inventory(
                    &inventory,
                    r1,
                    r1_sources,
                    oracle_native,
                    oracle_sources,
                    input.producer.clone(),
                    &basis,
                    encoding,
                )?;
                if TEST_REJOIN_ORACLE_MISMATCH.get()
                    || !(serde_json::to_vec(&outcome.publication)?
                        == serde_json::to_vec(&full.publication)?
                        && serde_json::to_vec(&outcome.report)?
                            == serde_json::to_vec(&full.report)?)
                {
                    return Err(RejoinOracleMismatch.into());
                }
            }
            sem::validate_evidence_with_attested_batch(
                &outcome.publication,
                &validated_native,
                &captured,
                raw,
                &inventory,
                &attested,
            )?;
            Ok((outcome.publication, captured))
        })();
        check_cancel(cancel)?;
        let (publication, captured) = match attempt {
            Ok(good) => good,
            Err(error) if rejoin_storage_failure(&error) => {
                return Err(rejoin_storage_error(error));
            }
            #[cfg(feature = "test-support")]
            Err(error) if error.is::<RejoinOracleMismatch>() => return Err(error),
            Err(error) if input.lightweight => {
                return Err(if error.is::<WitnessFallback>() {
                    error
                } else {
                    WitnessFallback("retained_input_unavailable").into()
                });
            }
            Err(error) => {
                let diagnostic = format!("selected raw capture unavailable: {error:#}");
                let diagnostic: String = diagnostic.chars().take(512).collect();
                let requested: BTreeSet<_> = input.requested_documents.iter().collect();
                let coverage = native
                    .revision
                    .documents
                    .iter()
                    .filter_map(|doc| {
                        let language: sem::Language =
                            serde_json::from_value(serde_json::json!(doc.key.language)).ok()?;
                        input
                            .producer
                            .languages
                            .contains(&language)
                            .then(|| sem::Coverage {
                                producer_id: input.producer.id.clone(),
                                language,
                                source_set_id: doc.key.source_set_id.clone(),
                                document_path: doc.key.path.clone(),
                                revision_id: native.revision.id.clone(),
                                requested: requested.contains(&doc.key),
                                selected: requested.contains(&doc.key),
                                state: if requested.contains(&doc.key) {
                                    sem::CoverageState::Failed
                                } else {
                                    sem::CoverageState::NotRequested
                                },
                                supported_roles: vec![],
                                observed_roles: vec![],
                                diagnostic: requested
                                    .contains(&doc.key)
                                    .then(|| diagnostic.clone()),
                            })
                    })
                    .collect();
                (
                    sem::SemanticPublication {
                        producers: vec![input.producer.clone()],
                        coverage,
                        provenance: vec![],
                        symbols: vec![],
                        declaration_bindings: vec![],
                        references: vec![],
                        call_bindings: vec![],
                        type_relationships: vec![],
                    },
                    sem::CapturedComponents {
                        producer_executables: BTreeMap::new(),
                        artifacts: BTreeMap::new(),
                        sources: BTreeMap::new(),
                        captured_revision: native.revision.clone(),
                        native_toolchain: serde_json::Value::Null,
                        native_config: vec![],
                        native_dependencies: vec![],
                        requested_producers: vec![],
                        toolchain: vec![],
                        config: vec![],
                        dependency: vec![],
                        native_extraction_components: BTreeMap::new(),
                    },
                )
            }
        };
        let mut no_requested = captured;
        no_requested.requested_producers.clear();
        write_import_semantic(tx, &key, &publication, &no_requested)?;
    }
    Ok(())
}

/// Records one producer descriptor, its languages and its binding to revision `key`. An
/// existing descriptor must be identical.
fn write_producer_descriptor(
    db: &Connection,
    key: &str,
    producer: &crate::semantic_evidence::Producer,
) -> Result<()> {
    db.execute(
        "INSERT OR IGNORE INTO semantic_producers VALUES(?1,?2,?3,?4,?5,?6)",
        params![
            producer.id,
            producer.version,
            producer.executable_hash,
            semantic_variant(&producer.kind)?,
            semantic_variant(&producer.position_encoding)?,
            producer_identity_kind(&producer.id),
        ],
    )?;
    let descriptor: (String,String,String,String) = db.query_row(
        "SELECT executable_hash,kind,position_encoding,identity_kind FROM semantic_producers WHERE id=?1 AND version=?2",
        params![producer.id,producer.version], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    )?;
    ensure!(
        descriptor
            == (
                producer.executable_hash.clone(),
                semantic_variant(&producer.kind)?,
                semantic_variant(&producer.position_encoding)?,
                producer_identity_kind(&producer.id).to_owned()
            ),
        "incompatible_index: semantic producer descriptor changed"
    );
    for (ordinal, language) in producer.languages.iter().enumerate() {
        db.execute(
            "INSERT OR IGNORE INTO semantic_producer_languages VALUES(?1,?2,?3,?4)",
            params![
                producer.id,
                producer.version,
                semantic_variant(language)?,
                ordinal as i64
            ],
        )?;
    }
    let persisted: Vec<String> = semantic_ordered(
        db,
        "SELECT ordinal,language FROM semantic_producer_languages WHERE id=?1 AND version=?2 ORDER BY ordinal",
        &[&producer.id, &producer.version],
    )?;
    let supplied = producer
        .languages
        .iter()
        .map(semantic_variant)
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        persisted == supplied,
        "incompatible_index: semantic producer languages changed"
    );
    db.execute(
        "INSERT INTO semantic_revision_producers VALUES(?1,?2,?3)",
        params![key, producer.id, producer.version],
    )?;
    Ok(())
}

fn write_semantic(
    db: &Connection,
    native: &crate::native_evidence::Artifact,
    prior: Option<IndexPin>,
    pin: IndexPin,
    supplied: Option<(
        &crate::semantic_evidence::SemanticPublication,
        &crate::semantic_evidence::CapturedComponents,
    )>,
    selected: &[SelectedNativeInput],
) -> Result<()> {
    let key = semantic_pin_key(pin);
    let prior_key = prior
        .map(semantic_pin_key)
        .map(|previous| -> Result<_> {
            let source_set: String = db.query_row(
                "SELECT source_set_id FROM native_revisions WHERE id=?1",
                [&previous],
                |row| row.get(0),
            )?;
            Ok((source_set == native.revision.source_set_id).then_some(previous))
        })
        .transpose()?
        .flatten();
    let same_native = if let Some(previous) = &prior_key {
        let old: Option<(String, String)> = db
            .query_row(
                "SELECT native_revision_id,source_inventory FROM native_revisions WHERE id=?1",
                [previous],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        old.is_some_and(|(id, inventory)| {
            id == native.revision.id
                && inventory
                    == serde_json::to_string(&native.revision.documents).unwrap_or_default()
        })
    } else {
        false
    };
    if let Some((publication, captured)) = supplied {
        for producer in &publication.producers {
            write_producer_descriptor(db, &key, producer)?;
        }
        for producer in &captured.requested_producers {
            db.execute(
                "INSERT INTO semantic_requested_producers VALUES(?1,?2,?3,?4)",
                params![key, producer.id, producer.version, producer.executable_hash],
            )?;
            for (ordinal, language) in producer.languages.iter().enumerate() {
                db.execute(
                    "INSERT INTO semantic_requested_producer_languages VALUES(?1,?2,?3,?4)",
                    params![
                        key,
                        producer.id,
                        semantic_variant(language)?,
                        ordinal as i64
                    ],
                )?;
            }
        }
        for coverage in &publication.coverage {
            let producer = publication
                .producers
                .iter()
                .find(|p| p.id == coverage.producer_id)
                .context("missing semantic producer")?;
            db.execute("INSERT INTO semantic_coverage(revision_id,producer_id,producer_version,language,path,requested,selected,state,diagnostic) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)", params![
                key,coverage.producer_id,producer.version,semantic_variant(&coverage.language)?,coverage.document_path,
                coverage.requested,coverage.selected,semantic_variant(&coverage.state)?,coverage.diagnostic,
            ])?;
            let tuple = db.last_insert_rowid();
            for (kind, roles) in [
                ("supported", &coverage.supported_roles),
                ("observed", &coverage.observed_roles),
            ] {
                for (ordinal, role) in roles.iter().enumerate() {
                    db.execute(
                        "INSERT INTO semantic_coverage_roles VALUES(?1,?2,?3,?4)",
                        params![tuple, kind, ordinal as i64, semantic_variant(role)?],
                    )?;
                }
            }
            write_semantic_tuple(db, tuple, coverage, publication)?;
        }
    }
    if let Some(previous) = prior_key {
        db.execute(
            "INSERT INTO semantic_revision_producers SELECT ?1,id,version FROM semantic_revision_producers old WHERE revision_id=?2 AND NOT EXISTS(SELECT 1 FROM semantic_revision_producers current WHERE current.revision_id=?1 AND current.id=old.id)",
            params![key, previous],
        )?;
        if supplied.is_none() {
            db.execute(
                "INSERT INTO semantic_requested_producers SELECT ?1,id,version,executable_hash FROM semantic_requested_producers WHERE revision_id=?2",
                params![key, previous],
            )?;
            db.execute(
                "INSERT INTO semantic_requested_producer_languages SELECT ?1,id,language,ordinal FROM semantic_requested_producer_languages WHERE revision_id=?2",
                params![key, previous],
            )?;
            if selected.is_empty() {
                copy_semantic_rows(db, &previous, &key, same_native)?;
            } else {
                for input in selected {
                    db.execute("INSERT INTO semantic_revision_raw_selections SELECT ?1,producer_id,batch_id,captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?2 AND producer_id=?3", params![key, previous, input.producer.id])?;
                }
                copy_semantic_rows_filtered(db, &previous, &key, same_native, None, selected)?;
            }
        } else {
            fill_semantic_coverage(db, &previous, &key)?;
        }
    }
    Ok(())
}

fn write_semantic_tuple(
    db: &Connection,
    tuple: i64,
    coverage: &crate::semantic_evidence::Coverage,
    publication: &crate::semantic_evidence::SemanticPublication,
) -> Result<()> {
    use crate::semantic_evidence as sem;
    let matches = |p: &sem::Provenance| {
        p.producer_id == coverage.producer_id
            && p.document.language == semantic_variant(&coverage.language).unwrap_or_default()
            && p.document.path == coverage.document_path
    };
    let mut basis_ids = BTreeMap::new();
    let mut provenance = BTreeSet::new();
    for proof in publication.provenance.iter().filter(|p| matches(p)) {
        let basis = proof
            .basis
            .as_ref()
            .context("missing captured semantic basis")?;
        let encoded = serde_json::to_string(basis)?;
        let basis_id = if let Some(&id) = basis_ids.get(&encoded) {
            id
        } else {
            execute_cached(
                db,
                "INSERT INTO semantic_basis(tuple_id,producer_id,producer_version,producer_hash,artifact_hash,language,source_set_id,captured_native_revision_id,source_manifest_hash,toolchain_hash,config_hash,dependency_hash) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    tuple,
                    basis.producer_id,
                    basis.producer_version,
                    basis.producer_hash,
                    basis.artifact_hash,
                    semantic_variant(&basis.language)?,
                    basis.source_set_id,
                    basis.revision_id,
                    basis.source_manifest_hash,
                    basis.toolchain_hash,
                    basis.config_hash,
                    basis.dependency_hash,
                ],
            )?;
            let id = db.last_insert_rowid();
            for (ordinal, lookup) in basis.lookup_dependencies.iter().enumerate() {
                execute_cached(
                    db,
                    "INSERT INTO semantic_basis_lookup_dependencies VALUES(?1,?2,?3)",
                    params![id, ordinal as i64, lookup],
                )?;
            }
            basis_ids.insert(encoded, id);
            id
        };
        let derived = proof.derived_from.as_ref();
        let capture_id: Option<i64> = if derived.is_some() {
            Some(query_row_cached(
                db,
                "SELECT capture_id FROM semantic_raw_captures WHERE producer_id=?1 AND artifact_hash=?2 AND source_set_id=?3 AND captured_revision=?4",
                params![
                    proof.producer_id,
                    basis.artifact_hash,
                    basis.source_set_id,
                    basis.revision_id
                ],
                |row| row.get(0),
            )?)
        } else {
            None
        };
        execute_cached(
            db,
            "INSERT INTO semantic_provenance(tuple_id,provenance_id,content_hash,evidence_kind,freshness,basis_id,capture_id,derived_from_provenance_id,derived_from_record_kind,derived_from_fact_digest) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                tuple,
                proof.id,
                proof.content_hash,
                semantic_variant(&proof.evidence_kind)?,
                semantic_variant(&proof.freshness)?,
                basis_id,
                capture_id,
                derived.map(|d| &d.provenance_id),
                derived
                    .map(|d| semantic_variant(&d.record_kind))
                    .transpose()?,
                derived.map(|d| &d.fact_digest),
            ],
        )?;
        provenance.insert(proof.id.as_str());
    }
    let owns = |proof: &str| provenance.contains(proof);
    for (ordinal, symbol) in publication
        .symbols
        .iter()
        .filter(|s| owns(&s.provenance_id))
        .enumerate()
    {
        execute_cached(
            db,
            "INSERT INTO semantic_symbols(tuple_id,ordinal,key_scheme,key_symbol,key_scope,key_document_language,key_document_path,display_name,provenance_id) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                tuple,
                ordinal as i64,
                semantic_variant(&symbol.key.scheme)?,
                symbol.key.symbol,
                semantic_variant(&symbol.key.scope)?,
                symbol.key.document.as_ref().map(|d| d.language.as_str()),
                symbol.key.document.as_ref().map(|d| d.path.as_str()),
                symbol.display_name,
                symbol.provenance_id,
            ],
        )?;
        let id = db.last_insert_rowid();
        for (ordinal, target) in symbol.declarations.iter().enumerate() {
            insert_semantic_target(
                db,
                "semantic_symbol_targets",
                &[id.into(), (ordinal as i64).into()],
                target,
            )?;
        }
    }
    for (ordinal, binding) in publication
        .declaration_bindings
        .iter()
        .filter(|b| owns(&b.provenance_id))
        .enumerate()
    {
        let anchor = &binding.join.anchor;
        execute_cached(
            db,
            "INSERT INTO semantic_declaration_bindings(tuple_id,ordinal,syntax_id,provenance_id,join_document_language,join_document_path,join_native_revision_id,join_content_hash,join_start,join_end,join_kind,join_status,join_diagnostic) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                tuple,
                ordinal as i64,
                binding
                    .syntax_id
                    .as_ref()
                    .map(|id| id_codec::encode_sid(id)
                        .context("invalid semantic syntax ID")
                        .map(|b| b.to_vec()))
                    .transpose()?,
                binding.provenance_id,
                anchor.document.language,
                anchor.document.path,
                anchor.revision_id,
                anchor.content_hash,
                anchor.range.start as i64,
                anchor.range.end as i64,
                semantic_variant(&anchor.kind)?,
                semantic_variant(&binding.join.status)?,
                binding.join.diagnostic,
            ],
        )?;
        let id = db.last_insert_rowid();
        for (ordinal, key) in binding.symbols.iter().enumerate() {
            execute_cached(
                db,
                "INSERT INTO semantic_declaration_binding_symbols VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    id,
                    ordinal as i64,
                    semantic_variant(&key.scheme)?,
                    key.symbol,
                    semantic_variant(&key.scope)?,
                    key.document.as_ref().map(|d| d.language.as_str()),
                    key.document.as_ref().map(|d| d.path.as_str()),
                ],
            )?;
        }
        for (ordinal, candidate) in binding.join.candidate_ids.iter().enumerate() {
            execute_cached(
                db,
                "INSERT INTO semantic_declaration_join_candidates VALUES(?1,?2,?3)",
                params![
                    id,
                    ordinal as i64,
                    id_codec::encode_sid(candidate)
                        .context("invalid semantic syntax ID")?
                        .to_vec(),
                ],
            )?;
        }
    }
    for reference in publication
        .references
        .iter()
        .filter(|r| owns(&r.provenance_id))
    {
        let id = id_codec::encode_occ(&reference.id)
            .context("invalid semantic reference ID")?
            .to_vec();
        let owner = id_codec::encode_sid(&reference.owner_syntax_id)
            .context("invalid semantic owner ID")?
            .to_vec();
        let mut values: Vec<rusqlite::types::Value> = vec![
            tuple.into(),
            id.clone().into(),
            (reference.ordinal as i64).into(),
            owner.into(),
            (reference.range.start as i64).into(),
            (reference.range.end as i64).into(),
            reference.spelling.clone().into(),
            reference.lookup_key.clone().into(),
            semantic_variant(&reference.site)?.into(),
            semantic_variant(&reference.resolution)?.into(),
        ];
        values.extend(if let Some(target) = &reference.declared_target {
            semantic_target_values(target)?
        } else {
            vec![rusqlite::types::Value::Null; 10]
        });
        values.push(reference.provenance_id.clone().into());
        execute_cached(
            db,
            &format!(
                "INSERT INTO semantic_references VALUES({})",
                (1..=values.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            rusqlite::params_from_iter(values),
        )?;
        for (ordinal, role) in reference.roles.iter().enumerate() {
            execute_cached(
                db,
                "INSERT INTO semantic_reference_roles VALUES(?1,?2,?3,?4)",
                params![tuple, id, ordinal as i64, semantic_variant(role)?],
            )?;
        }
        for (ordinal, target) in reference.candidates.iter().enumerate() {
            insert_semantic_target(
                db,
                "semantic_reference_candidates",
                &[tuple.into(), id.clone().into(), (ordinal as i64).into()],
                target,
            )?;
        }
    }
    for (ordinal, binding) in publication
        .call_bindings
        .iter()
        .filter(|b| owns(&b.provenance_id))
        .enumerate()
    {
        let anchor = &binding.join.anchor;
        let mut values: Vec<rusqlite::types::Value> = vec![
            tuple.into(),
            (ordinal as i64).into(),
            binding
                .call_id
                .as_ref()
                .map(|id| {
                    id_codec::encode_occ(id)
                        .context("invalid semantic call ID")
                        .map(|b| rusqlite::types::Value::Blob(b.to_vec()))
                })
                .transpose()?
                .unwrap_or(rusqlite::types::Value::Null),
            anchor.document.language.clone().into(),
            anchor.document.path.clone().into(),
            anchor.revision_id.clone().into(),
            anchor.content_hash.clone().into(),
            (anchor.range.start as i64).into(),
            (anchor.range.end as i64).into(),
            semantic_variant(&anchor.kind)?.into(),
            semantic_variant(&binding.join.status)?.into(),
            binding.join.diagnostic.clone().into(),
            semantic_variant(&binding.resolution)?.into(),
        ];
        values.extend(if let Some(target) = &binding.declared_target {
            semantic_target_values(target)?
        } else {
            vec![rusqlite::types::Value::Null; 10]
        });
        values.extend([
            semantic_variant(&binding.dispatch)?.into(),
            (binding.possible_dispatch_complete as i64).into(),
            binding.stale_target.map(|value| value as i64).into(),
            binding.provenance_id.clone().into(),
        ]);
        execute_cached(
            db,
            &format!(
                "INSERT INTO semantic_call_bindings(tuple_id,ordinal,call_id,join_document_language,join_document_path,join_native_revision_id,join_content_hash,join_start,join_end,join_kind,join_status,join_diagnostic,resolution,declared_kind,declared_internal_syntax_id,declared_internal_language,declared_internal_path,declared_internal_native_revision_id,declared_external_scheme,declared_external_symbol,declared_external_scope,declared_external_language,declared_external_path,dispatch,possible_dispatch_complete,stale_target,provenance_id) VALUES({})",
                (1..=values.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            rusqlite::params_from_iter(values),
        )?;
        let id = db.last_insert_rowid();
        for (table, targets) in [
            ("semantic_call_candidates", &binding.candidates),
            (
                "semantic_call_possible_dispatch",
                &binding.possible_dispatch,
            ),
        ] {
            for (ordinal, target) in targets.iter().enumerate() {
                insert_semantic_target(db, table, &[id.into(), (ordinal as i64).into()], target)?;
            }
        }
        for (ordinal, candidate) in binding.join.candidate_ids.iter().enumerate() {
            execute_cached(
                db,
                "INSERT INTO semantic_call_join_candidates VALUES(?1,?2,?3)",
                params![
                    id,
                    ordinal as i64,
                    id_codec::encode_occ(candidate)
                        .context("invalid semantic candidate ID")?
                        .to_vec()
                ],
            )?;
        }
    }
    for (ordinal, relationship) in publication
        .type_relationships
        .iter()
        .filter(|r| owns(&r.provenance_id))
        .enumerate()
    {
        let mut values: Vec<rusqlite::types::Value> = vec![
            tuple.into(),
            (ordinal as i64).into(),
            semantic_variant(&relationship.kind)?.into(),
        ];
        values.extend(semantic_target_values(&relationship.source)?);
        values.extend(semantic_target_values(&relationship.target)?);
        values.push(relationship.provenance_id.clone().into());
        execute_cached(
            db,
            &format!(
                "INSERT INTO semantic_type_relationships VALUES(NULL,{})",
                (1..=values.len())
                    .map(|i| format!("?{i}"))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            rusqlite::params_from_iter(values),
        )?;
    }
    Ok(())
}

fn release_semantic_tuple(db: &Connection, revision: &str) -> Result<bool> {
    let tuple: Option<i64> = db
        .query_row(
            "SELECT tuple_id FROM semantic_coverage WHERE revision_id=?1 ORDER BY tuple_id LIMIT 1",
            [revision],
            |row| row.get(0),
        )
        .optional()?;
    let Some(tuple) = tuple else {
        for table in [
            "semantic_import_reports",
            "semantic_revision_raw_selections",
            "semantic_requested_producer_languages",
            "semantic_requested_producers",
            "semantic_revision_producers",
        ] {
            let removed = db.execute(
                &format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE revision_id=?1 LIMIT 64)"),
                [revision],
            )?;
            if removed > 0 {
                return Ok(true);
            }
        }
        let proofs = db.execute("DELETE FROM semantic_raw_proofs WHERE rowid IN (SELECT p.rowid FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE NOT EXISTS(SELECT 1 FROM semantic_provenance v WHERE v.capture_id=p.capture_id AND v.derived_from_provenance_id=p.proof_id) AND NOT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s WHERE s.batch_id=c.batch_id) LIMIT 64)",[])?;
        let captures = db.execute("DELETE FROM semantic_raw_captures WHERE capture_id IN (SELECT c.capture_id FROM semantic_raw_captures c WHERE NOT EXISTS(SELECT 1 FROM semantic_raw_proofs p WHERE p.capture_id=c.capture_id) AND NOT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s WHERE s.batch_id=c.batch_id) LIMIT 64)",[])?;
        let exclusions = db.execute("DELETE FROM semantic_raw_exclusions WHERE rowid IN (SELECT e.rowid FROM semantic_raw_exclusions e WHERE NOT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s WHERE s.batch_id=e.batch_id) LIMIT 64)",[])?;
        let batches = db.execute("DELETE FROM semantic_raw_batches WHERE batch_id IN (SELECT b.batch_id FROM semantic_raw_batches b WHERE NOT EXISTS(SELECT 1 FROM semantic_raw_captures c WHERE c.batch_id=b.batch_id) AND NOT EXISTS(SELECT 1 FROM semantic_raw_exclusions e WHERE e.batch_id=b.batch_id) AND NOT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s WHERE s.batch_id=b.batch_id) LIMIT 64)",[])?;
        return Ok(proofs > 0 || captures > 0 || exclusions > 0 || batches > 0);
    };
    for (table, scope) in [
        ("semantic_reference_roles", "tuple_id=?1"),
        ("semantic_reference_candidates", "tuple_id=?1"),
        ("semantic_references", "tuple_id=?1"),
        (
            "semantic_symbol_targets",
            "symbol_id IN (SELECT symbol_id FROM semantic_symbols WHERE tuple_id=?1)",
        ),
        (
            "semantic_declaration_binding_symbols",
            "binding_id IN (SELECT binding_id FROM semantic_declaration_bindings WHERE tuple_id=?1)",
        ),
        (
            "semantic_declaration_join_candidates",
            "binding_id IN (SELECT binding_id FROM semantic_declaration_bindings WHERE tuple_id=?1)",
        ),
        (
            "semantic_call_candidates",
            "binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE tuple_id=?1)",
        ),
        (
            "semantic_call_possible_dispatch",
            "binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE tuple_id=?1)",
        ),
        (
            "semantic_call_join_candidates",
            "binding_id IN (SELECT binding_id FROM semantic_call_bindings WHERE tuple_id=?1)",
        ),
        ("semantic_type_relationships", "tuple_id=?1"),
        ("semantic_symbols", "tuple_id=?1"),
        ("semantic_declaration_bindings", "tuple_id=?1"),
        ("semantic_call_bindings", "tuple_id=?1"),
        ("semantic_provenance", "tuple_id=?1"),
        (
            "semantic_basis_lookup_dependencies",
            "basis_id IN (SELECT basis_id FROM semantic_basis WHERE tuple_id=?1)",
        ),
        ("semantic_basis", "tuple_id=?1"),
        ("semantic_coverage_roles", "tuple_id=?1"),
    ] {
        let removed = db.execute(&format!("DELETE FROM {table} WHERE rowid IN (SELECT rowid FROM {table} WHERE {scope} LIMIT 64)"),[tuple])?;
        if removed > 0 {
            return Ok(true);
        }
    }
    db.execute("DELETE FROM semantic_coverage WHERE tuple_id=?1", [tuple])?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn write_native(
    db: &Connection,
    bundle: (
        &crate::native_evidence::Artifact,
        &crate::capture::Capture,
        &Graph,
        &crate::classes::Catalog,
        &BTreeMap<String, crate::model::FileExtraction>,
    ),
    revision: IndexPin,
    stats: &IndexStats,
    cancel: &CancelFlag,
    immutable: &mut ImmutableAppend,
    reuse: &PreflightReuse,
    rows: &BTreeMap<&str, PublicationRows<'_>>,
    full_diagnostics: &[Diagnostic],
    class_complete: bool,
    mode: NativeWrite<'_>,
) -> Result<BTreeMap<String, V8DocumentProjection>> {
    let (write_header, ordinal_start, captured_file, precomputed) = match mode {
        NativeWrite::Header { ordinal_start } => (true, ordinal_start, None, None),
        NativeWrite::Document {
            ordinal,
            captured_file,
            precomputed,
        } => (false, ordinal, Some(captured_file), Some(precomputed)),
    };
    let (artifact, capture, graph, classes, extractions) = bundle;
    let a = artifact;
    let revision_key = format!(
        "pin:v1:{}:{}",
        revision.index_generation, revision.index_revision
    );
    if write_header {
        let (retained_toolchain, retained_config, retained_dependencies) =
            crate::native_evidence::captured_components(capture, &a.producer.executable_hash)?;
        for (expected, canonical) in [
            (
                &a.revision.toolchain_hash,
                crate::native_ids::canonical(&retained_toolchain),
            ),
            (
                &a.revision.config_hash,
                crate::native_ids::canonical(&serde_json::json!(retained_config)),
            ),
            (
                &a.revision.dependency_hash,
                crate::native_ids::canonical(&serde_json::json!(retained_dependencies)),
            ),
        ] {
            ensure!(
                expected == &crate::native_ids::digest(b"", &canonical),
                "native_evidence_required: retained component differs from measured revision"
            );
        }
        let origin: Option<(String,String,String,String,String)> = db.query_row(
        "SELECT id,version,executable_hash,kind,position_encoding FROM native_producers LIMIT 1",
        [], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
    ).optional()?;
        if let Some((id, version, _, kind, encoding)) = &origin {
            ensure!(
                id == &a.producer.id
                    && version == &a.producer.version
                    && kind == &a.producer.kind
                    && encoding == &a.producer.position_encoding,
                "incompatible_index: native producer schema/extractor descriptor differs"
            );
        } else {
            immutable.insert(
                db,
                "INSERT INTO native_producers VALUES(?1,?2,?3,?4,?5)",
                params![
                    a.producer.id,
                    a.producer.version,
                    a.producer.executable_hash,
                    a.producer.kind,
                    a.producer.position_encoding
                ],
            )?;
        }
        // The immutable row is the generation origin. Each NEW revision records the
        // validated executing hash in revision_capture_inputs + its SHA binding.

        for (ordinal, language) in a.producer.languages.iter().enumerate() {
            immutable.insert(
                db,
                "INSERT INTO native_producer_languages VALUES(?1,?2,?3,?4,?5)",
                params![
                    a.producer.id,
                    a.producer.version,
                    language,
                    1,
                    ordinal as i64
                ],
            )?;
        }
        immutable.insert(
            db,
            "INSERT INTO native_source_sets VALUES(?1,?2)",
            params![a.source_set.id, a.source_set.root_id],
        )?;
        for (ordinal, language) in a.source_set.languages.iter().enumerate() {
            immutable.insert(
                db,
                "INSERT INTO native_source_set_languages VALUES(?1,?2,?3)",
                params![a.source_set.id, language, ordinal as i64],
            )?;
        }
        for (ordinal, dependency) in a.source_set.dependencies.iter().enumerate() {
            immutable.insert(
                db,
                "INSERT INTO native_source_set_dependencies VALUES(?1,?2,?3)",
                params![a.source_set.id, dependency, ordinal as i64],
            )?;
        }
        immutable.insert_manifest(
            db,
            "INSERT INTO native_revisions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
            params![
                revision_key,
                a.revision.source_set_id,
                a.revision.toolchain_hash,
                a.revision.config_hash,
                a.revision.dependency_hash,
                a.revision.id,
                json(&a.revision.documents)?,
                json(&a.source_set.dependencies)?,
                json(&retained_toolchain)?,
                json(&retained_config)?,
                json(&retained_dependencies)?,
                json(capture.reconcile_options())?,
                json(&classes.warnings)?,
                classes.truncated,
                json(stats)?,
                json(full_diagnostics)?,
                revision.index_revision as i64,
            ],
        )?;
        for (key, observation) in capture.persisted_inputs()? {
            check_cancel(cancel)?;
            ensure!(
                key != "__released:v1" && key != "__pending_release:v1",
                "native_evidence_required: reserved release marker"
            );
            immutable.insert_manifest(
                db,
                "INSERT INTO revision_capture_inputs VALUES(?1,?2,?3)",
                params![revision_key, key, json(&observation)?],
            )?;
        }
    }
    let selected_documents = if write_header {
        a.revision
            .documents
            .get(ordinal_start..ordinal_start.saturating_add(graph.files.len()))
            .context("native document ordinal outside revision header")?
    } else {
        ensure!(
            graph.files.len() == 1 && a.revision.documents.len() == 1,
            "native_evidence_required: streamed document inventory differs"
        );
        &a.revision.documents
    };
    let documents: BTreeMap<_, _> = selected_documents
        .iter()
        .map(|doc| (doc.key.path.as_str(), doc))
        .collect();
    let coverages: BTreeMap<_, _> = a
        .coverage
        .iter()
        .map(|coverage| (coverage.document_path.as_str(), coverage))
        .collect();
    let captured_by_key: HashMap<(&str, &str), &SourceFile> =
        if captured_file.is_none() && !graph.files.is_empty() {
            capture
                .files
                .iter()
                .map(|source| ((source.language.as_str(), source.path.as_str()), source))
                .collect()
        } else {
            HashMap::new()
        };
    // Projection IDs and native witnesses are pure per-document values. They
    // are computed on workers before the insert loop, under the same reuse
    // conditions, and each is consumed below exactly where it was computed
    // before, so every earlier check still runs first. SQL stays here.
    let mut precomputed = match precomputed {
        // A streamed caller already computed this one document on a worker.
        Some(one) => vec![Some(one)],
        None => crate::parallel::map_ordered(
            &graph.files,
            crate::parallel::default_workers(),
            cancel,
            |_, file| {
                precompute_document(
                    file,
                    a,
                    rows.get(file.path.as_str()),
                    documents.get(file.path.as_str()).copied(),
                    coverages.get(file.path.as_str()).copied(),
                    reuse,
                )
            },
        ),
    }
    .into_iter();
    let mut projections = BTreeMap::new();
    let mut storage_ids = BTreeMap::new();
    for (offset, file) in graph.files.iter().enumerate() {
        let ordinal = ordinal_start + offset;
        check_cancel(cancel)?;
        let (mut projection, mut witness) = precomputed.next().flatten().unwrap_or_default();
        let doc = documents
            .get(file.path.as_str())
            .context("native document missing")?;
        ensure!(
            doc.key.language == file.language
                && doc.content_hash == file.hash
                && doc.byte_length == file.text.len(),
            "native document bytes differ from captured graph"
        );
        let source = captured_file
            .or_else(|| {
                captured_by_key
                    .get(&(file.language.as_str(), file.path.as_str()))
                    .copied()
            })
            .filter(|source| *source == file)
            .context("native source missing immutable capture")?;
        ensure!(
            source.hash == file.hash,
            "native captured source hash differs"
        );
        let grouped = rows
            .get(file.path.as_str())
            .context("publication rows missing")?;
        let ids = if let Some(existing) = reuse.existing_ids.get(&file.path) {
            V8DocumentProjection {
                version_id: existing.version_id.clone(),
                graph_id: existing.graph_id.clone(),
                class_id: existing.class_id.clone(),
                graph_hash: existing.graph_hash.clone(),
                class_hash: existing.class_hash.clone(),
            }
        } else {
            projection
                .take()
                .unwrap_or_else(|| v8_document_projection(file, a, grouped))?
        };
        let context = crate::native_evidence::native_extraction_context(
            &a.producer,
            &file.language,
            &a.revision,
        )?;
        if !reuse.for_path(&file.path).native {
            immutable.insert(
                db,
                "INSERT INTO document_versions(id,language,path,content_hash,extraction_context,producer_id,producer_version,byte_length,source_bytes,native_witness) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    ids.version_id,
                    file.language,
                    file.path,
                    file.hash,
                    context,
                    a.producer.id,
                    a.producer.version,
                    file.text.len() as i64,
                    file.text.as_bytes(),
                    match witness.take() {
                        Some(witness) => witness?,
                        None => publication_native_witness(
                            doc,
                            &a.producer,
                            coverages
                                .get(file.path.as_str())
                                .context("missing native coverage")?,
                            grouped,
                        )?,
                    },
                ],
            )?;
        }
        let vid: i64 = db
            .prepare_cached("SELECT vid FROM document_versions WHERE id=?1")?
            .query_row([&ids.version_id], |row| row.get(0))?;
        if !reuse.for_path(&file.path).graph {
            immutable.insert(
                db,
                "INSERT INTO graph_projections(vid,language,graph_hash,state,class_extraction_state,class_extraction_payload,class_summary_payload) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    vid,
                    file.language,
                    ids.graph_hash,
                    "ready",
                    if matches!(file.language.as_str(), "java" | "python") {
                        "ready"
                    } else {
                        "notApplicable"
                    },
                    extractions.get(&file.path).map(json).transpose()?,
                    extractions.get(&file.path).map(|extraction| json(&extraction.summary())).transpose()?,
                ],
            )?;
        }
        let gid: i64 = db
            .prepare_cached("SELECT gid FROM graph_projections WHERE graph_hash=?1")?
            .query_row([&ids.graph_hash], |row| row.get(0))?;
        if !reuse.for_path(&file.path).class {
            immutable.insert(
                db,
                "INSERT INTO class_projections(gid,content_hash,state) VALUES(?1,?2,?3)",
                params![gid, ids.class_hash, "ready"],
            )?;
        }
        let cid: i64 = db
            .prepare_cached("SELECT cid FROM class_projections WHERE content_hash=?1")?
            .query_row([&ids.class_hash], |row| row.get(0))?;
        storage_ids.insert(file.path.clone(), (vid, gid, cid));
        let coverage = coverages
            .get(file.path.as_str())
            .context("missing native coverage")?;
        ensure!(
            coverage.source_set_id == a.source_set.id
                && coverage.language == file.language
                && coverage.revision_id == a.revision.id,
            "native coverage document mismatch"
        );
        immutable.insert_manifest(
            db,
            "INSERT INTO revision_documents VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                revision_key,
                file.language,
                file.path,
                vid,
                gid,
                cid,
                json(&capture.source_stat(&file.path)?)?,
                coverage.requested,
                coverage.selected,
                coverage.state,
                coverage.diagnostic,
                ordinal as i64,
                class_complete || !matches!(file.language.as_str(), "java" | "python"),
            ],
        )?;
        for (kind, roles) in [
            ("supported", &coverage.supported_roles),
            ("observed", &coverage.observed_roles),
        ] {
            for (ordinal, role) in roles.iter().enumerate() {
                if reuse.for_path(&file.path).native {
                    continue;
                }
                immutable.insert(
                    db,
                    "INSERT INTO native_version_coverage_roles VALUES(?1,?2,?3,?4)",
                    params![vid, kind, role, ordinal as i64],
                )?;
            }
        }
        ensure!(
            projections.insert(file.path.clone(), ids).is_none(),
            "duplicate projected path"
        );
    }
    let mut owners = BTreeMap::new();
    for d in &a.declarations {
        let key = (d.document.path.clone(), json(&d.ancestors)?, json(&d.key)?);
        ensure!(
            owners.insert(key, d.syntax_id.clone()).is_none(),
            "duplicate native owner key"
        );
    }
    for d in &a.declarations {
        check_cancel(cancel)?;
        let vid = storage_ids
            .get(&d.document.path)
            .context("native declaration missing document")?
            .0;
        if reuse.for_path(&d.document.path).native {
            continue;
        }
        let owner = d
            .ancestors
            .last()
            .map(|last| -> Result<String> {
                let prefix = json(&d.ancestors[..d.ancestors.len() - 1])?;
                let key = json(last)?;
                owners
                    .get(&(d.document.path.clone(), prefix.clone(), key.clone()))
                    .cloned()
                    .with_context(||format!("native declaration parent missing: path={} syntax={} ancestor={} key={}",d.document.path,d.syntax_id,prefix,key))
            })
            .transpose()?;
        let sig = d.key.signature.as_ref();
        immutable.insert(db,"INSERT INTO native_version_declarations VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",params![
            vid,stored_sid(&d.syntax_id)?,owner.as_deref().map(stored_sid).transpose()?,d.kind,d.role.as_str(),d.name,d.lookup_key,sig.is_some(),sig.map(|s|s.type_parameter_count as i64),
            sig.map(|s|s.variadic),d.key.ordinal as i64,d.range.start as i64,d.range.end as i64,
            d.name_range.as_ref().map(|r|r.start as i64),d.name_range.as_ref().map(|r|r.end as i64),
        ])?;
        if let Some(sig) = sig {
            for (ordinal, parameter) in sig.parameter_types.iter().enumerate() {
                immutable.insert(
                    db,
                    "INSERT INTO native_version_own_signature_types VALUES(?1,?2,?3,?4)",
                    params![vid, stored_sid(&d.syntax_id)?, ordinal as i64, parameter],
                )?;
            }
        }
        for (ordinal, key) in d.ancestors.iter().enumerate() {
            let sig = key.signature.as_ref();
            immutable.insert(db,"INSERT INTO native_version_declaration_ancestors VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![
                vid,stored_sid(&d.syntax_id)?,ordinal as i64,key.kind,key.name,key.ordinal as i64,sig.is_some(),
                sig.map(|s|s.type_parameter_count as i64),sig.map(|s|s.variadic),
            ])?;
            if let Some(sig) = sig {
                for (parameter_ordinal, parameter) in sig.parameter_types.iter().enumerate() {
                    immutable.insert(db,"INSERT INTO native_version_ancestor_signature_types VALUES(?1,?2,?3,?4,?5)",params![
                        vid,stored_sid(&d.syntax_id)?,ordinal as i64,parameter_ordinal as i64,parameter,
                    ])?;
                }
            }
        }
        immutable.insert(
            db,
            "INSERT INTO native_version_headers VALUES(?1,?2,?3,?4,?5)",
            params![
                vid,
                stored_sid(&d.syntax_id)?,
                d.header.kind,
                d.header.name,
                d.header.result_type,
            ],
        )?;
        for (kind, items) in [
            ("modifier", &d.header.modifiers),
            ("typeParameter", &d.header.type_parameters),
            ("base", &d.header.bases),
        ] {
            for (ordinal, item) in items.iter().enumerate() {
                immutable.insert(
                    db,
                    "INSERT INTO native_version_header_items VALUES(?1,?2,?3,?4,?5)",
                    params![vid, stored_sid(&d.syntax_id)?, kind, ordinal as i64, item],
                )?;
            }
        }
        for (ordinal, parameter) in d.header.parameters.iter().enumerate() {
            immutable.insert(
                db,
                "INSERT INTO native_version_parameters VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    vid,
                    stored_sid(&d.syntax_id)?,
                    ordinal as i64,
                    parameter.name,
                    parameter.type_name,
                    parameter.variadic,
                ],
            )?;
        }
    }
    search::write_cards(db, a, &storage_ids, reuse, immutable, cancel)?;
    for report in &a.capability_reports {
        check_cancel(cancel)?;
        if reuse.for_path(&report.document.path).native {
            continue;
        }
        let vid = storage_ids
            .get(&report.document.path)
            .context("native capability missing document")?
            .0;
        for (ordinal, family) in report.families.iter().enumerate() {
            let kind = semantic_variant(&family.kind)?;
            immutable.insert(
                db,
                "INSERT INTO native_version_capability_families VALUES(?1,?2,?3,?4,?5)",
                params![
                    vid,
                    kind,
                    family.available,
                    family.diagnostic,
                    ordinal as i64
                ],
            )?;
            for (gap_ordinal, gap) in family.gaps.iter().enumerate() {
                immutable.insert(
                    db,
                    "INSERT INTO native_version_capability_gaps VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        vid,
                        kind,
                        gap_ordinal as i64,
                        gap.range.start as i64,
                        gap.range.end as i64,
                        gap.diagnostic
                    ],
                )?;
            }
        }
    }
    for reference in &a.references {
        check_cancel(cancel)?;
        if reuse.for_path(&reference.document.path).native {
            continue;
        }
        let vid = storage_ids
            .get(&reference.document.path)
            .context("native reference missing document")?
            .0;
        immutable.insert(
            db,
            "INSERT INTO native_version_references VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                vid,
                stored_occ(&reference.id)?,
                stored_sid(&reference.owner_syntax_id)?,
                reference.ordinal as i64,
                reference.range.start as i64,
                reference.range.end as i64,
                reference.spelling,
                (reference.lookup_key != reference.spelling).then_some(&reference.lookup_key)
            ],
        )?;
    }
    for r in &a.control_regions {
        check_cancel(cancel)?;
        if reuse.for_path(&r.document.path).native {
            continue;
        }
        let vid = storage_ids
            .get(&r.document.path)
            .context("native region missing document")?
            .0;
        immutable.insert(
            db,
            "INSERT INTO native_version_control_regions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                vid,
                stored_occ(&r.id)?,
                stored_sid(&r.owner_syntax_id)?,
                r.ordinal as i64,
                r.kind,
                r.range.start as i64,
                r.range.end as i64,
                r.parent_id.as_deref().map(stored_occ).transpose()?,
                r.arm,
            ],
        )?;
    }
    let documents_by_key: BTreeMap<_, _> = selected_documents
        .iter()
        .map(|document| (&document.key, document))
        .collect();
    let mut stored_sources: BTreeMap<i64, String> = BTreeMap::new();
    for c in &a.calls {
        check_cancel(cancel)?;
        if reuse.for_path(&c.document.path).native {
            continue;
        }
        let vid = storage_ids
            .get(&c.document.path)
            .context("native call missing document")?
            .0;
        let document = documents_by_key
            .get(&c.document)
            .copied()
            .context("native call missing validated document")?;
        // These versions were inserted above from the admitted captured files.
        // Streamed writes borrow that text; plain artifacts read and decode each
        // call-bearing version once. Reused native versions skip this loop.
        let source = if let Some(file) = captured_file {
            file.text.as_str()
        } else {
            match stored_sources.entry(vid) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut().as_str(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    #[cfg(test)]
                    WRITER_SOURCE_SQL_READS.with(|reads| reads.set(reads.get() + 1));
                    let bytes: Vec<u8> = db.query_row(
                        "SELECT source_bytes FROM document_versions WHERE vid=?1 AND language=?2 AND path=?3",
                        params![vid, document.key.language, document.key.path],
                        |row| row.get(0),
                    )?;
                    let source = String::from_utf8(bytes)
                        .map_err(|error| error.utf8_error())
                        .context("native stored source is not UTF-8")?;
                    entry.insert(source).as_str()
                }
            }
        };
        let callee_lookup_key =
            crate::native_evidence::verified_callee_lookup(&document.key.language, c, source);
        immutable.insert(
            db,
            "INSERT INTO native_version_calls VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            params![
                vid,
                stored_occ(&c.id)?,
                stored_sid(&c.owner_syntax_id)?,
                c.ordinal as i64,
                c.range.start as i64,
                c.range.end as i64,
                c.callee_range.as_ref().map(|r| r.start as i64),
                c.callee_range.as_ref().map(|r| r.end as i64),
                c.spelling,
                document.key.language,
                callee_lookup_key,
            ],
        )?;
        for (ordinal, region) in c.region_ids.iter().enumerate() {
            immutable.insert(
                db,
                "INSERT INTO native_version_call_regions VALUES(?1,?2,?3,?4)",
                params![vid, stored_occ(&c.id)?, stored_occ(region)?, ordinal as i64],
            )?;
        }
    }
    // Deferred FKs are enforced by COMMIT. A full foreign_key_check here would
    // read every unchanged occurrence while holding BEGIN IMMEDIATE.
    Ok(projections)
}

/// Bounded readiness check for a pair installed by the verified writer. This does not
/// attest every stored BLOB after out-of-band SQLite mutation. Ordinary Store open stays
/// bounded; pinned source reads verify the selected BLOB inside their read snapshot.
/// Schema-9 revision zero is a deliberately empty bootstrap, not a published
/// native pair. Refuse any staged evidence that appears before the first commit.
fn validate_v8_bootstrap(db: &Connection) -> Result<()> {
    let (indexed_at, options): (String, Option<String>) = db.query_row(
        "SELECT indexed_at,reconcile_options FROM index_metadata WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    control_ensure!(
        indexed_at.is_empty() && options.is_none(),
        "incompatible_index: invalid v8 bootstrap metadata"
    );
    // Fixed identifiers from CACHE_SCHEMA_V29 only. Do not read table names from
    // sqlite_master or treat a partial publication as a valid empty bootstrap.
    for name in [
        "class_projections",
        "class_relations",
        "classes",
        "document_versions",
        "graph_display_labels",
        "graph_nodes",
        "graph_projections",
        "native_producer_inputs",
        "native_producer_languages",
        "native_producers",
        "native_revisions",
        "native_source_set_dependencies",
        "native_source_set_languages",
        "native_source_sets",
        "native_version_ancestor_signature_types",
        "native_version_call_regions",
        "native_version_references",
        "native_version_capability_gaps",
        "native_version_capability_families",
        "native_version_calls",
        "native_version_control_regions",
        "native_version_coverage_roles",
        "native_version_declaration_ancestors",
        "native_version_declarations",
        "native_version_search_cards",
        "head_search_fts",
        "native_version_header_items",
        "native_version_headers",
        "native_version_own_signature_types",
        "native_version_parameters",
        "revision_capture_inputs",
        "revision_documents",
        "semantic_producers",
        "semantic_producer_languages",
        "semantic_revision_producers",
        "semantic_requested_producers",
        "semantic_requested_producer_languages",
        "semantic_coverage",
        "semantic_coverage_roles",
        "semantic_basis",
        "semantic_basis_lookup_dependencies",
        "semantic_provenance",
        "semantic_symbols",
        "semantic_symbol_targets",
        "semantic_declaration_bindings",
        "semantic_declaration_binding_symbols",
        "semantic_declaration_join_candidates",
        "semantic_references",
        "semantic_reference_roles",
        "semantic_reference_candidates",
        "semantic_call_bindings",
        "semantic_call_candidates",
        "semantic_call_possible_dispatch",
        "semantic_call_join_candidates",
        "semantic_type_relationships",
    ] {
        let present: i64 =
            db.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {name})"), [], |row| {
                row.get(0)
            })?;
        control_ensure!(present == 0, "incompatible_index: partial v8 bootstrap");
    }
    Ok(())
}

const GRAPH_FIELD_MAX_BYTES: i64 = 4 * 1024 * 1024;
const GRAPH_PAIR_MAX_BYTES: i64 = 6 * 1024 * 1024;

fn bounded_graph_pair(db: &Connection, sql: &str) -> Result<()> {
    let (stats_type, stats_len, diagnostics_type, diagnostics_len): (String, i64, String, i64) = db
        .query_row(sql, [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?;
    control_ensure!(
        stats_type == "text"
            && diagnostics_type == "text"
            && (0..=GRAPH_FIELD_MAX_BYTES).contains(&stats_len)
            && (0..=GRAPH_FIELD_MAX_BYTES).contains(&diagnostics_len)
            && stats_len + diagnostics_len <= GRAPH_PAIR_MAX_BYTES,
        "incompatible_index: graph metadata field or pair budget exceeded"
    );
    Ok(())
}

/// Unkeyed, recomputable consistency binding for post-upgrade revisions.
/// Publication has already authenticated the actual executable and raw capture;
/// selected reads compare only immutable persisted SHA identities and header.
/// This does not resist a coordinated rewrite of all fields by the same user.
fn revision_producer_binding(db: &Connection, revision_key: &str) -> Result<(String, String)> {
    let (source_set, native_revision, toolchain, config, dependency, published): (
        String,
        String,
        String,
        String,
        String,
        i64,
    ) = db.query_row(
        "SELECT source_set_id,native_revision_id,toolchain_hash,config_hash,
                dependency_hash,published_index_revision FROM native_revisions WHERE id=?1",
        [revision_key],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        },
    )?;
    selected_ensure!(
        native_revision.starts_with("revision:v2:")
            && [toolchain.as_str(), config.as_str(), dependency.as_str()]
                .iter()
                .all(|hash| hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))),
        "incompatible_index: invalid selected producer header"
    );
    let (producer_id, producer_version, origin_hash, kind, encoding): (
        String,
        String,
        String,
        String,
        String,
    ) = db.query_row(
        "SELECT id,version,executable_hash,kind,position_encoding FROM native_producers LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    )?;
    selected_ensure!(
        producer_id == crate::native_evidence::PRODUCER
            && producer_version == crate::native_evidence::NATIVE_VERSION
            && kind == "native"
            && encoding == "utf8",
        "incompatible_index: selected native producer descriptor differs"
    );
    let mut identities = Vec::new();
    let mut selectors = BTreeSet::new();
    let mut executable = None;
    let mut rows = db.prepare(
        "SELECT input_key,payload FROM revision_capture_inputs WHERE revision_id=?1
         AND (input_key LIKE 'toolchain:%' OR input_key LIKE 'executable:%') ORDER BY input_key",
    )?;
    for row in rows.query_map([revision_key], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (key, raw) = row?;
        selected_ensure!(
            key.len() <= 8192 && raw.len() <= 8192,
            "incompatible_index: selected producer input byte budget exceeded"
        );
        let observation: crate::capture::CaptureInputObservation = serde_json::from_str(&raw)
            .context(SelectedIntegrity(
                "incompatible_index: stored JSON decode failed".into(),
            ))?;
        selected_ensure!(
            json(&observation)? == raw,
            "incompatible_index: noncanonical producer input"
        );
        if let Some(relative) = key.strip_prefix("toolchain:") {
            selected_ensure!(
                crate::capture::ROOT_INPUTS[22..].contains(&relative)
                    && selectors.insert(relative.to_owned())
                    && matches!(
                        observation,
                        crate::capture::CaptureInputObservation::Present { .. }
                            | crate::capture::CaptureInputObservation::Absent
                    ),
                "incompatible_index: selected producer selector role differs"
            );
        } else if let Some(path) = key.strip_prefix("executable:") {
            let crate::capture::CaptureInputObservation::Present { ref hash, .. } = observation
            else {
                return Err(SelectedIntegrity(
                    "incompatible_index: selected executable input absent".into(),
                )
                .into());
            };
            selected_ensure!(
                Path::new(path).is_absolute()
                    && hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && executable.replace(hash.clone()).is_none(),
                "incompatible_index: selected executable input differs"
            );
        } else {
            return Err(SelectedIntegrity(
                "incompatible_index: selected producer input role differs".into(),
            )
            .into());
        }
        identities.push(serde_json::json!({"key":key,"observation":observation}));
    }
    selected_ensure!(
        selectors.len() == crate::capture::ROOT_INPUTS[22..].len()
            && identities.len() == selectors.len() + 1,
        "incompatible_index: selected producer input inventory incomplete"
    );
    let executable = executable.context(SelectedIntegrity(
        "incompatible_index: selected executable input missing".into(),
    ))?;
    let canonical = crate::native_ids::canonical(&serde_json::json!({
        "revisionKey":revision_key, "sourceSetId":source_set,
        "nativeRevisionId":native_revision, "publishedIndexRevision":published,
        "toolchainHash":toolchain, "configHash":config, "dependencyHash":dependency,
        "producerId":producer_id, "producerVersion":producer_version,
        "originExecutableHash":origin_hash, "kind":kind, "positionEncoding":encoding,
        "executingHash":executable, "selectorsAndExecutable":identities,
    }));
    Ok((
        executable,
        crate::native_ids::digest(b"trellis.revision-producer-binding.v1\0", &canonical),
    ))
}

fn selected_producer_hash(db: &Connection, selected: &ReadRevision) -> Result<String> {
    #[cfg(test)]
    count_selected_read(|counts| counts.producer_hashes += 1);
    let (executable, derived) = revision_producer_binding(db, &selected.key)?;
    let origin: String = db.query_row(
        "SELECT executable_hash FROM native_producers LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let epoch: Vec<(String, i64)> = db
        .prepare("SELECT index_generation,first_revision FROM native_binding_epoch LIMIT 2")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let head_revision: i64 = db.query_row(
        "SELECT index_revision FROM index_metadata WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    selected_ensure!(
        epoch.len() == 1
            && epoch[0].0 == selected.pin.index_generation.to_string()
            && epoch[0].1 >= 1
            && epoch[0].1 <= head_revision,
        "incompatible_index: selected producer binding epoch invalid"
    );
    let persisted: Option<(String, String)> = db
        .query_row(
            "SELECT producer_sha,binding_sha FROM revision_producer_bindings WHERE revision_id=?1",
            [&selected.key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if selected.pin.index_revision < epoch[0].1 as u64 {
        selected_ensure!(
            executable == origin && persisted.is_none(),
            "incompatible_index: legacy selected producer provenance differs"
        );
    } else {
        let (producer_sha, binding_sha) = persisted.ok_or_else(|| {
            SelectedIntegrity(
                "incompatible_index: selected revision producer binding missing".into(),
            )
        })?;
        selected_ensure!(
            producer_sha == executable && binding_sha == derived,
            "incompatible_index: selected revision producer binding differs"
        );
    }
    Ok(executable)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum PairedManifestScope {
    // A new Store admits the entire retained history once; staged replacement does too.
    Full,
    // Authored revisions were checked when published. Recheck the head, its newly
    // superseded predecessor, and in-flight maintenance rather than every manifest.
    Changing,
    // Private publication preflight only. Store::open has admitted the prior
    // history; the same publication checks the new head under its writer lock
    // before COMMIT. Never use this scope for a public status or staged index.
    Preflight,
}

// The schema-12 covering index lets this header query avoid wide JSON rows.
const HEADER_SQL: &str = "SELECT count(r.id),min(r.published_index_revision),max(r.published_index_revision),
            coalesce(sum(CASE WHEN r.id IS NULL THEN 0 WHEN r.id='pin:v1:'||m.index_generation||':'||r.published_index_revision
              AND r.source_set_id=?1 AND r.published_index_revision BETWEEN 1 AND m.index_revision
              THEN 0 ELSE 1 END),0),m.index_revision
         FROM index_metadata m LEFT JOIN native_revisions r ON true WHERE m.singleton=1";

fn paired_manifest_query(scope: PairedManifestScope) -> String {
    // Full admission walks every revision. During ordinary publication the
    // small set of mutable keys drives primary-key lookups, not a history scan.
    let candidates = match scope {
        PairedManifestScope::Full => "SELECT id FROM native_revisions",
        PairedManifestScope::Preflight => unreachable!("preflight cannot validate paired manifests"),
        PairedManifestScope::Changing => "SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1
           UNION SELECT 'pin:v1:'||index_generation||':'||(index_revision-1) FROM index_metadata WHERE singleton=1 AND index_revision>1
           UNION SELECT revision_id FROM native_revision_supersessions WHERE state='pending'
           UNION SELECT revision_id FROM native_revision_release_debt",
    };
    format!(
        "WITH checked_ids(id) AS MATERIALIZED ({candidates})
             SELECT EXISTS(SELECT 1 FROM checked_ids chosen
             CROSS JOIN index_metadata h
             LEFT JOIN native_revisions r ON r.id=chosen.id
             LEFT JOIN native_revision_supersessions s ON s.revision_id=r.id
             LEFT JOIN native_revision_release_debt debt ON debt.revision_id=r.id
             WHERE h.singleton=1 AND (r.id IS NULL OR
               (r.id='pin:v1:'||h.index_generation||':'||h.index_revision
                    AND (s.revision_id IS NOT NULL OR debt.revision_id IS NOT NULL
                         OR (SELECT count(*) FROM revision_documents d WHERE d.revision_id=r.id)
                              != json_array_length(r.source_inventory)))
               OR (r.id!='pin:v1:'||h.index_generation||':'||h.index_revision
                    AND s.revision_id IS NULL)
               OR (debt.revision_id IS NOT NULL AND (s.state!='pending' OR debt.phase!='pending'))
               OR (s.state='pending' AND debt.revision_id IS NULL)
               OR (s.state='retained' AND (debt.revision_id IS NOT NULL
                    OR EXISTS(SELECT 1 FROM revision_capture_inputs i WHERE i.revision_id=r.id
                      AND i.input_key IN ('__pending_release:v1','__released:v1'))
                    OR (SELECT count(*) FROM revision_documents d WHERE d.revision_id=r.id)
                       != json_array_length(r.source_inventory)))
               OR (s.state='pending' AND (NOT EXISTS(SELECT 1 FROM revision_capture_inputs i
                         WHERE i.revision_id=r.id AND i.input_key='__pending_release:v1'
                           AND i.payload='pending_release:v1')
                    OR EXISTS(SELECT 1 FROM revision_capture_inputs i WHERE i.revision_id=r.id
                        AND i.input_key='__released:v1')
                    OR (SELECT count(*) FROM revision_documents d WHERE d.revision_id=r.id)
                       > json_array_length(r.source_inventory)))
               OR (s.state='released' AND (debt.revision_id IS NOT NULL
                    OR (SELECT count(*) FROM revision_capture_inputs i WHERE i.revision_id=r.id)!=1
                    OR NOT EXISTS(SELECT 1 FROM revision_capture_inputs i WHERE i.revision_id=r.id
                         AND i.input_key='__released:v1' AND i.payload='released:v1')
                    OR EXISTS(SELECT 1 FROM revision_documents d WHERE d.revision_id=r.id)))
               OR EXISTS(SELECT 1 FROM revision_capture_inputs i WHERE i.revision_id=r.id
                    AND ((i.input_key='__pending_release:v1' AND (i.payload!='pending_release:v1'
                          OR s.state!='pending'))
                         OR (i.input_key='__released:v1' AND (i.payload!='released:v1'
                          OR s.state!='released'))))))"
    )
}

#[cfg(test)]
thread_local! {
    static PAIRED_CHECK_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn validate_paired_metadata(
    db: &Connection,
    root_id: &str,
    scope: PairedManifestScope,
) -> Result<()> {
    #[cfg(test)]
    PAIRED_CHECK_COUNT.with(|count| count.set(count.get() + 1));
    fn one_row(db: &Connection, sql: &str) -> Result<Option<(String, String)>> {
        let mut rows = db
            .prepare(sql)?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        control_ensure!(
            rows.len() <= 1,
            "incompatible_index: duplicate native pair metadata"
        );
        Ok(rows.pop())
    }
    let producer = one_row(db, "SELECT id,kind FROM native_producers LIMIT 2")?;
    // Occurrence IDs bind the native producer version (Decision 0003). An index persisted by
    // another native producer version holds IDs this binary cannot reproduce, so it is never
    // served; the integrity failure forces an in-place rebaseline from fresh measurement.
    let producer_version = one_row(db, "SELECT id,version FROM native_producers LIMIT 2")?;
    control_ensure!(
        producer_version
            .as_ref()
            .is_none_or(|(_, version)| version == crate::native_evidence::NATIVE_VERSION),
        "incompatible_index: native producer version mismatch"
    );
    let source = one_row(db, "SELECT id,root_id FROM native_source_sets LIMIT 2")?;
    let revision = one_row(
        db,
        "SELECT r.native_revision_id,r.source_set_id FROM native_revisions r JOIN index_metadata m ON m.singleton=1 AND r.published_index_revision=m.index_revision AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision LIMIT 2",
    )?;
    let expected_source = format!("source-set:v1:{root_id}");
    control_ensure!(
        producer
            .as_ref()
            .is_some_and(|(id, kind)| id == "trellis.native.syntax" && kind == "native")
            && source
                .as_ref()
                .is_some_and(|(id, root)| id == &expected_source && root == root_id)
            && revision.as_ref().is_some_and(
                |(id, source)| id.starts_with("revision:v2:") && source == &expected_source
            ),
        "incompatible_index: missing native pair metadata"
    );
    // The planner uses the narrow covering index from the base schema.
    let (count, first, last, mismatched, current): (i64, Option<i64>, Option<i64>, i64, i64) =
        db.query_row(HEADER_SQL, [&expected_source], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?;
    // Released revisions retain their headers and an FK-bound release marker.
    // A missing header remains corruption, never permission to skip a pin.
    control_ensure!(
        count == current && first == Some(1) && last == Some(current) && mismatched == 0,
        "incompatible_index: native header pin mismatch"
    );
    let malformed_sql = paired_manifest_query(scope);
    let malformed: bool = db.query_row(&malformed_sql, [], |row| row.get(0))?;
    control_ensure!(
        !malformed,
        "incompatible_index: retained manifest/release mismatch"
    );
    let stray_candidates: bool = db.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM native_release_candidate_versions c
             LEFT JOIN native_revision_supersessions s ON s.revision_id=c.revision_id
             WHERE s.state IS NOT 'pending'
           UNION ALL
           SELECT 1 FROM native_release_candidate_graphs c
             LEFT JOIN native_revision_supersessions s ON s.revision_id=c.revision_id
             WHERE s.state IS NOT 'pending'
           UNION ALL
           SELECT 1 FROM native_release_candidate_classes c
             LEFT JOIN native_revision_supersessions s ON s.revision_id=c.revision_id
             WHERE s.state IS NOT 'pending'
         )",
        [],
        |row| row.get(0),
    )?;
    control_ensure!(
        !stray_candidates,
        "incompatible_index: release candidates without pending debt"
    );
    bounded_graph_pair(
        db,
        "SELECT typeof(r.graph_stats),length(CAST(r.graph_stats AS BLOB)),
        typeof(r.graph_diagnostics),length(CAST(r.graph_diagnostics AS BLOB))
        FROM index_metadata m JOIN native_revisions r
          ON r.id='pin:v1:'||m.index_generation||':'||m.index_revision
             AND r.published_index_revision=m.index_revision WHERE m.singleton=1",
    )?;
    bounded_graph_pair(
        db,
        "SELECT typeof(stats),length(CAST(stats AS BLOB)),
        typeof(diagnostics),length(CAST(diagnostics AS BLOB))
        FROM index_metadata WHERE singleton=1",
    )?;
    let (header_stats, header_diagnostics, head_stats, head_diagnostics): (
        String,
        String,
        String,
        String,
    ) = db.query_row(
        "SELECT r.graph_stats,r.graph_diagnostics,m.stats,m.diagnostics
         FROM index_metadata m JOIN native_revisions r
           ON r.id='pin:v1:'||m.index_generation||':'||m.index_revision
              AND r.published_index_revision=m.index_revision WHERE m.singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    control_ensure!(
        header_stats == head_stats && header_diagnostics == head_diagnostics,
        "incompatible_index: native header/control graph metadata mismatch"
    );
    let _: IndexStats = serde_json::from_str(&header_stats).context(SelectedIntegrity(
        "incompatible_index: stored JSON decode failed".into(),
    ))?;
    let _: Vec<Diagnostic> = serde_json::from_str(&header_diagnostics).context(
        SelectedIntegrity("incompatible_index: stored JSON decode failed".into()),
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ScanComparison {
    pub(crate) examined: usize,
    pub(crate) added: usize,
    pub(crate) deleted: usize,
    pub(crate) stat_changed: usize,
    pub(crate) hash_changed: usize,
    pub(crate) ctime_changed: usize,
    pub(crate) unchanged_stat_input_hash_changed: usize,
    pub(crate) uncertain_timestamp_hashed: usize,
}
impl ScanComparison {
    fn changed(self) -> bool {
        self.added + self.deleted + self.stat_changed + self.hash_changed > 0
    }
}

pub(crate) type SourceObservations = BTreeMap<String, (String, crate::capture::CaptureStat)>;
pub(crate) type InputObservations = BTreeMap<String, crate::capture::CaptureInputObservation>;

pub(crate) fn compare_capture_observations(
    previous_sources: &SourceObservations,
    current_sources: &SourceObservations,
    previous_inputs: &InputObservations,
    current_inputs: &InputObservations,
) -> ScanComparison {
    let uncertain = |stat: &crate::capture::CaptureStat| {
        stat.mtime_seconds.is_none()
            || stat.mtime_nanoseconds.is_none()
            || stat.ctime_seconds.is_none()
            || stat.ctime_nanoseconds.is_none()
    };
    let mut comparison = ScanComparison::default();
    let source_keys: BTreeSet<_> = previous_sources
        .keys()
        .chain(current_sources.keys())
        .cloned()
        .collect();
    for key in &source_keys {
        comparison.examined += 1;
        match (previous_sources.get(key), current_sources.get(key)) {
            (None, Some(_)) => comparison.added += 1,
            (Some(_), None) => comparison.deleted += 1,
            (Some((old_hash, old_stat)), Some((new_hash, new_stat))) => {
                comparison.stat_changed += usize::from(old_stat != new_stat);
                comparison.hash_changed += usize::from(old_hash != new_hash);
                comparison.ctime_changed += usize::from(
                    old_stat.size == new_stat.size
                        && old_stat.device == new_stat.device
                        && old_stat.inode == new_stat.inode
                        && old_stat.mtime_seconds == new_stat.mtime_seconds
                        && old_stat.mtime_nanoseconds == new_stat.mtime_nanoseconds
                        && (old_stat.ctime_seconds != new_stat.ctime_seconds
                            || old_stat.ctime_nanoseconds != new_stat.ctime_nanoseconds),
                );
                comparison.uncertain_timestamp_hashed +=
                    usize::from(uncertain(old_stat) || uncertain(new_stat));
            }
            (None, None) => unreachable!(),
        }
    }
    let input_keys: BTreeSet<_> = previous_inputs
        .keys()
        .chain(current_inputs.keys())
        .cloned()
        .collect();
    for key in &input_keys {
        comparison.examined += 1;
        match (previous_inputs.get(key), current_inputs.get(key)) {
            (None, Some(_)) => comparison.added += 1,
            (Some(_), None) => comparison.deleted += 1,
            (Some(old), Some(new)) => match (old, new) {
                (
                    crate::capture::CaptureInputObservation::Present {
                        stat: old_stat,
                        hash: old_hash,
                    },
                    crate::capture::CaptureInputObservation::Present {
                        stat: new_stat,
                        hash: new_hash,
                    },
                ) => {
                    comparison.stat_changed += usize::from(old_stat != new_stat);
                    comparison.hash_changed += usize::from(old_hash != new_hash);
                    comparison.unchanged_stat_input_hash_changed +=
                        usize::from(old_stat == new_stat && old_hash != new_hash);
                    comparison.uncertain_timestamp_hashed +=
                        usize::from(uncertain(old_stat) || uncertain(new_stat));
                }
                (
                    crate::capture::CaptureInputObservation::Directory { stat: old },
                    crate::capture::CaptureInputObservation::Directory { stat: new },
                )
                | (
                    crate::capture::CaptureInputObservation::Root { stat: old },
                    crate::capture::CaptureInputObservation::Root { stat: new },
                ) => comparison.stat_changed += usize::from(old != new),
                (
                    crate::capture::CaptureInputObservation::Absent,
                    crate::capture::CaptureInputObservation::Absent,
                ) => {}
                (
                    crate::capture::CaptureInputObservation::Extractor { hash: old },
                    crate::capture::CaptureInputObservation::Extractor { hash: new },
                ) => comparison.hash_changed += usize::from(old != new),
                _ => comparison.stat_changed += 1,
            },
            (None, None) => unreachable!(),
        }
    }
    comparison
}

// Keep the selected manifest indexed even when many older revisions are retained.
// The revision join still verifies that this is the exact metadata-selected head.
const SELECTED_CAPTURE_SNAPSHOT_SQL: &str = "SELECT d.path,v.content_hash,d.capture_stat FROM revision_documents d JOIN document_versions v ON v.vid=d.vid JOIN native_revisions r ON r.id=d.revision_id JOIN index_metadata m ON m.index_revision=r.published_index_revision AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision WHERE d.revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) ORDER BY d.path";

fn compare_capture_snapshot(
    db: &Connection,
    capture: &crate::capture::Capture,
) -> Result<ScanComparison> {
    compare_capture_snapshot_with(db, capture, true)
}

/// UNCHANGED trusts the indexer's published source hashes and projection links.
/// Fresh Capture hashes are still compared; stored source BLOBs are not read.
fn compare_capture_snapshot_for_unchanged(
    db: &Connection,
    capture: &crate::capture::Capture,
    include_executable: bool,
    cancel: &CancelFlag,
) -> Result<ScanComparison> {
    compare_capture_snapshot_inner(db, capture, include_executable, Some(cancel))
}

fn compare_capture_snapshot_with(
    db: &Connection,
    capture: &crate::capture::Capture,
    include_executable: bool,
) -> Result<ScanComparison> {
    compare_capture_snapshot_inner(db, capture, include_executable, None)
}

fn compare_capture_snapshot_inner(
    db: &Connection,
    capture: &crate::capture::Capture,
    include_executable: bool,
    unchanged: Option<&CancelFlag>,
) -> Result<ScanComparison> {
    let mut previous_sources = BTreeMap::new();
    let sql = if unchanged.is_some() {
        "SELECT d.path,v.content_hash,d.capture_stat,v.language,v.path,v.byte_length,
            g.state,c.state,v.extraction_context,v.producer_id,v.producer_version
         FROM revision_documents d
         LEFT JOIN document_versions v ON v.vid=d.vid AND v.language=d.language AND v.path=d.path
         LEFT JOIN graph_projections g ON g.gid=d.gid AND g.vid=d.vid AND g.language=d.language
         LEFT JOIN class_projections c ON c.cid=d.cid AND c.gid=d.gid
         WHERE d.revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision
             FROM index_metadata WHERE singleton=1) ORDER BY d.path"
    } else {
        SELECTED_CAPTURE_SNAPSHOT_SQL
    };
    let mut statement = db.prepare(sql)?;
    let mut rows = statement.query([])?;
    let mut reuse_metadata = BTreeMap::new();
    while let Some(row) = rows.next()? {
        if let Some(cancel) = unchanged {
            check_cancel(cancel)?;
        }
        let path: String = row.get(0)?;
        let hash: Option<String> = row.get(1)?;
        let hash = hash.ok_or_else(|| {
            SelectedIntegrity("incompatible_index: selected reused source missing".into())
        })?;
        let stat: String = row.get(2)?;
        let stat: crate::capture::CaptureStat = serde_json::from_str(&stat).context(
            SelectedIntegrity("incompatible_index: stored JSON decode failed".into()),
        )?;
        if unchanged.is_some() {
            reuse_metadata.insert(
                path.clone(),
                (
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                ),
            );
        }
        selected_ensure!(
            previous_sources.insert(path, (hash, stat)).is_none(),
            "incompatible_index: duplicate captured source observation"
        );
    }
    let mut current_sources = BTreeMap::new();
    for source in &capture.files {
        current_sources.insert(
            source.path.clone(),
            (source.hash.clone(), capture.source_stat(&source.path)?),
        );
    }

    let mut previous_inputs = BTreeMap::new();
    let mut statement =
        db.prepare("SELECT i.input_key,i.payload FROM revision_capture_inputs i JOIN native_revisions r ON r.id=i.revision_id JOIN index_metadata m ON m.index_revision=r.published_index_revision AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision ORDER BY i.input_key")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (key, payload) = row?;
        let observation: crate::capture::CaptureInputObservation = serde_json::from_str(&payload)
            .context(SelectedIntegrity(
            "incompatible_index: stored JSON decode failed".into(),
        ))?;
        selected_ensure!(
            previous_inputs.insert(key, observation).is_none(),
            "incompatible_index: duplicate captured input observation"
        );
    }
    let mut current_inputs = capture.persisted_inputs()?;
    if !include_executable {
        previous_inputs.retain(|key: &String, _| !key.starts_with("executable:"));
        current_inputs.retain(|key, _| !key.starts_with("executable:"));
    }
    let comparison = compare_capture_observations(
        &previous_sources,
        &current_sources,
        &previous_inputs,
        &current_inputs,
    );
    if !comparison.changed()
        && let Some(cancel) = unchanged
    {
        for source in &capture.files {
            check_cancel(cancel)?;
            let (language, path, length, graph, class, context, producer, version) =
                reuse_metadata.get(&source.path).ok_or_else(|| {
                    SelectedIntegrity("incompatible_index: selected reused source missing".into())
                })?;
            selected_ensure!(
                language == &source.language
                    && path == &source.path
                    && usize::try_from(*length).ok() == Some(source.text.len()),
                "incompatible_index: selected reused source metadata differs"
            );
            selected_ensure!(
                graph.as_deref() == Some("ready")
                    && class.as_deref() == Some("ready")
                    && producer == crate::native_evidence::PRODUCER
                    && version == crate::native_evidence::NATIVE_VERSION
                    && context
                        == &crate::native_evidence::declared_selected_extraction_context(
                            producer,
                            version,
                            &source.language,
                        )?,
                "incompatible_index: selected reusable projection link or extraction context invalid"
            );
        }
    }
    Ok(comparison)
}

fn validate_capture_stat(stat: &crate::capture::CaptureStat, expected_kind: &str) -> Result<()> {
    let nanos = |value: Option<i64>| value.is_some_and(|n| (0..1_000_000_000).contains(&n));
    control_ensure!(
        stat.version == 1
            && stat.kind == expected_kind
            && stat.device.is_some() == stat.inode.is_some()
            && stat.mtime_seconds.is_some() == stat.mtime_nanoseconds.is_some()
            && stat.ctime_seconds.is_some() == stat.ctime_nanoseconds.is_some()
            && stat.mtime_seconds.is_some() == stat.ctime_seconds.is_some(),
        "incompatible_index: invalid capture stat"
    );
    #[cfg(unix)]
    control_ensure!(
        stat.device.is_some_and(|n| n > 0)
            && stat.inode.is_some_and(|n| n > 0)
            && nanos(stat.mtime_nanoseconds)
            && nanos(stat.ctime_nanoseconds),
        "incompatible_index: incomplete Unix capture stat"
    );
    #[cfg(not(unix))]
    control_ensure!(
        stat.device.is_none()
            && stat.inode.is_none()
            && stat.mtime_seconds.is_none()
            && stat.ctime_seconds.is_none(),
        "incompatible_index: invalid portable capture stat"
    );
    Ok(())
}

// The same selected-pin anchor keeps reconciliation independent of retained manifests.
const SELECTED_RECONCILE_FILES_SQL: &str = "SELECT d.path,d.capture_stat FROM revision_documents d JOIN native_revisions r ON r.id=d.revision_id JOIN index_metadata m ON m.index_revision=r.published_index_revision AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision WHERE d.revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) ORDER BY d.path";

fn validate_reconcile_inventory(db: &Connection) -> Result<()> {
    let options: Option<String> = db.query_row(
        "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    let options = options
        .ok_or_else(|| ControlIntegrity("incompatible_index: missing reconcile options".into()))?;
    let decoded_options: crate::indexer::ReconcileOptions = serde_json::from_str(&options)
        .context(SelectedIntegrity(
            "incompatible_index: stored JSON decode failed".into(),
        ))
        .context("incompatible_index: invalid reconcile options")?;
    control_ensure!(
        json(&decoded_options)? == options
            && decoded_options.version == 2
            && decoded_options.max_file_bytes > 0
            && decoded_options.max_file_bytes <= 256 * 1024 * 1024,
        "incompatible_index: unsupported reconcile options"
    );

    let mut source_paths = BTreeSet::new();
    let mut file_statement = db.prepare(SELECTED_RECONCILE_FILES_SQL)?;
    let mut files = file_statement.query([])?;
    while let Some(row) = files.next()? {
        let path: String = row.get(0)?;
        control_ensure!(
            !path.is_empty()
                && !Path::new(&path).is_absolute()
                && !path.split('/').any(|part| part.is_empty() || part == ".."),
            "incompatible_index: invalid captured source path"
        );
        control_ensure!(
            source_paths.insert(path.clone()),
            "incompatible_index: duplicate captured source path"
        );
        let payload: String = row.get(1)?;
        let decoded: crate::capture::CaptureStat = serde_json::from_str(&payload)
            .context(SelectedIntegrity(
                "incompatible_index: stored JSON decode failed".into(),
            ))
            .context("incompatible_index: invalid capture stat")?;
        validate_capture_stat(&decoded, "file")?;
        control_ensure!(
            json(&decoded)? == payload,
            "incompatible_index: invalid capture stat"
        );
    }

    let mut input_statement =
        db.prepare("SELECT i.input_key,i.payload FROM revision_capture_inputs i JOIN native_revisions r ON r.id=i.revision_id JOIN index_metadata m ON m.index_revision=r.published_index_revision AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision ORDER BY i.input_key")?;
    let mut inputs = input_statement.query([])?;
    let mut actual = BTreeSet::new();
    let mut directories = BTreeSet::new();
    let mut executable = None;
    while let Some(row) = inputs.next()? {
        let key: String = row.get(0)?;
        let payload: String = row.get(1)?;
        control_ensure!(
            !key.is_empty() && key.len() <= 8192 && actual.insert(key.clone()),
            "incompatible_index: invalid capture input key"
        );
        let observation: crate::capture::CaptureInputObservation = serde_json::from_str(&payload)
            .context(SelectedIntegrity(
                "incompatible_index: stored JSON decode failed".into(),
            ))
            .context("incompatible_index: invalid capture input")?;
        control_ensure!(
            json(&observation)? == payload,
            "incompatible_index: noncanonical capture input"
        );
        match (&key[..], &observation) {
            ("root:.", crate::capture::CaptureInputObservation::Root { stat }) => {
                validate_capture_stat(stat, "directory")?;
            }
            (key, crate::capture::CaptureInputObservation::Directory { stat })
                if key.starts_with("directory:") =>
            {
                validate_capture_stat(stat, "directory")?;
                let relative = key.trim_start_matches("directory:");
                control_ensure!(
                    relative.is_empty()
                        || (!Path::new(relative).is_absolute()
                            && !relative
                                .split('/')
                                .any(|part| part.is_empty() || part == "..")),
                    "incompatible_index: invalid captured directory"
                );
                directories.insert(relative.to_owned());
            }
            (key, crate::capture::CaptureInputObservation::Present { stat, hash })
                if key.starts_with("config:")
                    || key.starts_with("toolchain:")
                    || key.starts_with("ignore:")
                    || key.starts_with("executable:") =>
            {
                validate_capture_stat(stat, "file")?;
                control_ensure!(
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                    "incompatible_index: invalid capture input hash"
                );
                if key.starts_with("executable:") {
                    control_ensure!(
                        executable.replace(key.to_owned()).is_none(),
                        "incompatible_index: duplicate executable input"
                    );
                }
            }
            (key, crate::capture::CaptureInputObservation::Extractor { hash })
                if key == crate::capture::EXTRACTOR_INPUT_KEY =>
            {
                control_ensure!(
                    hash.len() == 64
                        && hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                    "incompatible_index: invalid extractor producer hash"
                );
            }
            (key, crate::capture::CaptureInputObservation::Absent)
                if key.starts_with("config:")
                    || key.starts_with("toolchain:")
                    || key.starts_with("ignore:") => {}
            _ => {
                return Err(ControlIntegrity(
                    "incompatible_index: capture input role mismatch".into(),
                )
                .into());
            }
        }
    }

    control_ensure!(
        directories.contains(""),
        "incompatible_index: missing root directory inventory"
    );
    for source in &source_paths {
        let mut parent = Path::new(source).parent();
        while let Some(path) = parent {
            let relative = path
                .to_str()
                .ok_or_else(|| {
                    ControlIntegrity("incompatible_index: non-UTF8 source parent".into())
                })?
                .replace('\\', "/");
            control_ensure!(
                directories.contains(&relative),
                "incompatible_index: missing source directory inventory"
            );
            parent = path.parent();
        }
    }
    let mut expected = BTreeSet::from(["root:.".to_owned()]);
    for name in &crate::capture::ROOT_INPUTS[..22] {
        expected.insert(format!("config:{name}"));
    }
    for name in &crate::capture::ROOT_INPUTS[22..] {
        expected.insert(format!("toolchain:{name}"));
    }
    for directory in &directories {
        for name in [".gitignore", ".ignore"] {
            expected.insert(if directory.is_empty() {
                format!("ignore:{name}")
            } else {
                format!("ignore:{directory}/{name}")
            });
        }
        expected.insert(format!("directory:{directory}"));
    }
    // Nested package roots of JavaScript sources (#443) and nested Cargo manifests of Rust
    // sources (#394); a non-regular one is not admitted.
    for source in &source_paths {
        let manifest = match Path::new(source).extension().and_then(|x| x.to_str()) {
            Some("js" | "mjs" | "cjs") => "package.json",
            Some("rs") => "Cargo.toml",
            _ => continue,
        };
        for dir in Path::new(source).ancestors().skip(1) {
            let key = format!("config:{}/{manifest}", dir.to_string_lossy());
            if !dir.as_os_str().is_empty() && actual.contains(&key) {
                expected.insert(key);
            }
        }
    }
    expected.insert(
        executable.ok_or_else(|| {
            ControlIntegrity("incompatible_index: missing executable input".into())
        })?,
    );
    // Revisions published before the extractor producer hash lack it; they
    // stay readable and simply never qualify for rebuilt-executable reuse.
    if actual.contains(crate::capture::EXTRACTOR_INPUT_KEY) {
        expected.insert(crate::capture::EXTRACTOR_INPUT_KEY.to_owned());
    }
    control_ensure!(
        actual == expected,
        "incompatible_index: incomplete or unknown capture inventory"
    );
    Ok(())
}

fn validate_bounded_control(
    db: &Connection,
    root_id: &str,
    scope: PairedManifestScope,
) -> Result<()> {
    validate_supersessions(db)?;
    bounded_graph_pair(
        db,
        "SELECT typeof(stats),length(CAST(stats AS BLOB)),
        typeof(diagnostics),length(CAST(diagnostics AS BLOB))
        FROM index_metadata WHERE singleton=1",
    )?;
    let (stats, diagnostics): (String, String) = db.query_row(
        "SELECT stats,diagnostics FROM index_metadata WHERE singleton=1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let _: IndexStats = serde_json::from_str(&stats).context(SelectedIntegrity(
        "incompatible_index: stored JSON decode failed".into(),
    ))?;
    let _: Vec<Diagnostic> = serde_json::from_str(&diagnostics).context(SelectedIntegrity(
        "incompatible_index: stored JSON decode failed".into(),
    ))?;
    if scope != PairedManifestScope::Preflight {
        validate_paired_metadata(db, root_id, scope)?;
    }
    validate_reconcile_inventory(db)?;
    let warnings_bytes: Option<i64> = db
        .query_row(
            "SELECT length(CAST(r.class_warnings AS BLOB)) FROM native_revisions r JOIN index_metadata m ON m.index_revision=r.published_index_revision AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision",
            [],
            |row| row.get(0),
        )
        .optional()?;
    selected_ensure!(
        warnings_bytes.is_some_and(|bytes| (0..=256 * 1024).contains(&bytes)),
        "incompatible_index: class catalog byte budget exceeded or missing"
    );
    Ok(())
}

fn validate_paired_rows(db: &Connection) -> Result<()> {
    validate_supersessions(db)?;
    let count = |table: &str| -> Result<i64> {
        Ok(db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    };
    selected_ensure!(
        count("native_producers")? == 1
            && count("native_source_sets")? == 1
            && count("native_revisions")? >= 1,
        "incompatible_index: missing native pair"
    );
    let (manifest_rows, projection_rows, inventory_rows): (i64, i64, i64) = db.query_row(
        "SELECT (SELECT count(*) FROM revision_documents d WHERE d.revision_id=r.id),
                (SELECT count(*) FROM revision_documents d WHERE d.revision_id=r.id AND d.cid IS NOT NULL),
                json_array_length(r.source_inventory)
         FROM native_revisions r JOIN index_metadata m ON m.singleton=1
           AND r.published_index_revision=m.index_revision
           AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    selected_ensure!(
        manifest_rows == inventory_rows && projection_rows == manifest_rows,
        "incompatible_index: incomplete revision manifest"
    );
    let mut stmt=db.prepare("SELECT d.source_bytes,d.content_hash,d.byte_length,r.path,r.language,n.source_set_id,g.language
        FROM revision_documents r JOIN native_revisions n ON n.id=r.revision_id
        JOIN document_versions d ON d.vid=r.vid
        JOIN graph_projections g ON g.gid=r.gid")?;
    let mut matched = 0;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        use sha2::{Digest, Sha256};
        let bytes: Vec<u8> = row.get(0)?;
        let hash: String = row.get(1)?;
        let length: i64 = row.get(2)?;
        let _ = String::from_utf8(bytes.clone()).context(SelectedIntegrity(
            "incompatible_index: stored source encoding".into(),
        ))?;
        selected_ensure!(
            length == bytes.len() as i64
                && hash == hex::encode(Sha256::digest(&bytes))
                && row.get::<_, String>(4)? == row.get::<_, String>(6)?
                && row.get::<_, String>(5)?.starts_with("source-set:v1:")
                && !row.get::<_, String>(3)?.is_empty(),
            "incompatible_index: manifest source mismatch"
        );
        matched += 1;
    }
    selected_ensure!(
        matched == count("revision_documents")?,
        "incompatible_index: missing manifest document"
    );
    selected_ensure!(
        db.prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_none(),
        "incompatible_index: native foreign key mismatch"
    );
    validate_reconcile_inventory(db)?;
    Ok(())
}

#[derive(Clone)]
struct V8NativeScope {
    revision_key: String, // internal pin:v1 generation/revision key
    version_id: String,
    vid: i64,
    key: crate::native_evidence::DocumentKey,
    native_revision_id: String,
    producer_id: String,
}
impl V8NativeScope {
    fn proof_id(&self) -> String {
        format!(
            "native-proof:v1:{}",
            crate::native_ids::digest(
                b"trellis.native-proof.v1\0",
                &crate::native_ids::canonical(&serde_json::json!({
                    "producerId":self.producer_id,"document":self.key,
                    "revisionId":self.native_revision_id
                })),
            )
        )
    }
}
// Decode compact ID columns at the SQLite row boundary. A malformed stored
// BLOB is a corrupt index, not a malformed caller selector.
fn stored_id_row(
    row: &rusqlite::Row<'_>,
    index: usize,
    occurrence: bool,
) -> rusqlite::Result<String> {
    let blob: Vec<u8> = row.get(index)?;
    let text = if occurrence {
        id_codec::decode_occ(&blob)
    } else {
        id_codec::decode_sid(&blob)
    };
    text.map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error.to_string(),
            )),
        )
    })
}
fn optional_stored_id_row(
    row: &rusqlite::Row<'_>,
    index: usize,
    occurrence: bool,
) -> rusqlite::Result<Option<String>> {
    if row.get::<_, Option<Vec<u8>>>(index)?.is_none() {
        return Ok(None);
    }
    stored_id_row(row, index, occurrence).map(Some)
}

fn v8_native_scope_for(
    db: &Connection,
    key: &crate::native_evidence::DocumentKey,
    selected: &ReadRevision,
) -> Result<Option<V8NativeScope>> {
    let mut stmt = db.prepare_cached(
        "SELECT rd.revision_id,d.id,d.vid,r.native_revision_id,d.producer_id
        FROM native_revisions r JOIN revision_documents rd ON rd.revision_id=r.id
        JOIN document_versions d ON d.vid=rd.vid
          AND d.language=rd.language AND d.path=rd.path
        WHERE r.id=?4 AND r.source_set_id=?1 AND rd.language=?2 AND rd.path=?3 LIMIT 2",
    )?;
    let rows = stmt
        .query_map(
            params![key.source_set_id, key.language, key.path, selected.key],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() <= 1,
        SelectedIntegrity("incompatible_index: duplicate selected document manifest".into())
    );
    Ok(rows.into_iter().next().map(
        |(revision_key, version_id, vid, native_revision_id, producer_id)| V8NativeScope {
            revision_key,
            version_id,
            vid,
            key: key.clone(),
            native_revision_id,
            producer_id,
        },
    ))
}

fn v8_owner_scope_for(
    db: &Connection,
    owner: &str,
    selected: &ReadRevision,
) -> Result<Option<V8NativeScope>> {
    use crate::native_evidence::DocumentKey;
    let mut stmt = db.prepare_cached(
        "SELECT r.source_set_id,m.language,m.path
        FROM native_version_declarations d JOIN revision_documents m
          ON m.revision_id=?2 AND m.vid=d.vid
        JOIN native_revisions r ON r.id=m.revision_id
        WHERE d.syntax_id=?1 LIMIT 2",
    )?;
    let Some(owner) = id_codec::encode_sid(owner) else {
        return Ok(None);
    };
    let keys: Vec<DocumentKey> = stmt
        .query_map(params![owner, selected.key], |r| {
            Ok(DocumentKey {
                source_set_id: r.get(0)?,
                language: r.get(1)?,
                path: r.get(2)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    ensure!(
        keys.len() <= 1,
        SelectedIntegrity("incompatible_index: duplicate native owner identity".into())
    );
    keys.first()
        .map(|key| {
            v8_native_scope_for(db, key, selected)?.ok_or_else(|| {
                SelectedIntegrity("incompatible_index: native owner manifest missing".into()).into()
            })
        })
        .transpose()
}
/// Decode rows already read from one snapshot, polling `progress`; it needs no connection.
trait Decode<T>: FnOnce(Option<&dyn Fn() -> bool>) -> Result<T> + Send {}
impl<T, F: FnOnce(Option<&dyn Fn() -> bool>) -> Result<T> + Send> Decode<T> for F {}
/// Pin a decoder's argument lifetime: any `progress` borrow, not one inferred.
fn decoder<T, F>(decode: F) -> F
where
    F: FnOnce(Option<&dyn Fn() -> bool>) -> Result<T>,
{
    decode
}
fn contiguous(ordinal: i64, expected: usize, what: &str) -> Result<()> {
    selected_ensure!(
        ordinal == i64::try_from(expected)?,
        "incompatible_index: {what} ordinal gap"
    );
    Ok(())
}
fn decode_signature(
    types: &mut BTreeMap<String, Vec<String>>,
    id: &str,
    present: bool,
    count: Option<i64>,
    variadic: Option<bool>,
) -> Result<Option<crate::native_evidence::Signature>> {
    let values = types.remove(id).unwrap_or_default();
    if !present {
        selected_ensure!(
            values.is_empty(),
            "incompatible_index: orphan native signature types"
        );
        return Ok(None);
    }
    Ok(Some(crate::native_evidence::Signature {
        parameter_types: values,
        type_parameter_count: usize::try_from(
            count.ok_or_else(|| SelectedIntegrity("missing native signature count".into()))?,
        )
        .map_err(|error| SelectedIntegrity(error.to_string()))?,
        variadic: variadic
            .ok_or_else(|| SelectedIntegrity("missing native signature variadic".into()))?,
    }))
}
// This reads the WHOLE selected version, checks every child, THEN filters lookup keys.
// It cannot accidentally skip bad children attached to a declaration outside a lookup filter.
fn read_native_declarations_v8(
    db: &Connection,
    scope: &V8NativeScope,
    lookup_key: Option<&str>,
    all_lookup_keys: bool,
) -> Result<Vec<crate::native_evidence::Declaration>> {
    read_native_declarations_v8_inner(db, scope, lookup_key, all_lookup_keys, None)
}

fn read_native_declarations_v8_optional(
    db: &Connection,
    scope: &V8NativeScope,
    lookup_key: Option<&str>,
    all_lookup_keys: bool,
    expired: &dyn Fn() -> bool,
) -> Result<Vec<crate::native_evidence::Declaration>> {
    read_native_declarations_v8_inner(db, scope, lookup_key, all_lookup_keys, Some(expired))
}

/// Read every row of one selected version; the returned decoder checks and
/// assembles them without the connection, so callers may decode in parallel.
fn fetch_native_declarations_v8(
    db: &Connection,
    scope: &V8NativeScope,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<impl Decode<Vec<crate::native_evidence::Declaration>> + use<>> {
    optional_hint_check(progress)?;
    let v = &scope.vid;
    let rows0 = db.prepare_cached("SELECT syntax_id,ordinal,type_name FROM native_version_own_signature_types WHERE vid=?1 ORDER BY syntax_id,ordinal")?
        .query_map([v],|r|Ok((stored_id_row(r,0,false)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows1 = db.prepare_cached("SELECT syntax_id,ancestor_ordinal,ordinal,type_name FROM native_version_ancestor_signature_types WHERE vid=?1 ORDER BY syntax_id,ancestor_ordinal,ordinal")?
        .query_map([v],|r|Ok((stored_id_row(r,0,false)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows2 = db.prepare_cached("SELECT syntax_id,ordinal,kind,name,sibling_ordinal,signature_present,type_parameter_count,variadic FROM native_version_declaration_ancestors WHERE vid=?1 ORDER BY syntax_id,ordinal")?
        .query_map([v],|r|Ok((stored_id_row(r,0,false)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,i64>(4)?,r.get::<_,bool>(5)?,r.get::<_,Option<i64>>(6)?,r.get::<_,Option<bool>>(7)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows3 = db.prepare_cached("SELECT syntax_id,kind,name,result_type FROM native_version_headers WHERE vid=?1 ORDER BY syntax_id")?
        .query_map([v],|r|Ok((stored_id_row(r,0,false)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows4 = db.prepare_cached("SELECT syntax_id,item_kind,ordinal,value FROM native_version_header_items WHERE vid=?1 ORDER BY syntax_id,item_kind,ordinal")?
        .query_map([v],|r|Ok((stored_id_row(r,0,false)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows5 = db.prepare_cached("SELECT syntax_id,ordinal,name,type_name,variadic FROM native_version_parameters WHERE vid=?1 ORDER BY syntax_id,ordinal")?
        .query_map([v],|r|Ok((stored_id_row(r,0,false)?,r.get::<_,i64>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,bool>(4)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows6 = db.prepare_cached("SELECT syntax_id,owner_syntax_id,kind,role,name,lookup_key,key_signature_present,key_type_parameter_count,key_variadic,key_ordinal,start_byte,end_byte,name_start,name_end FROM native_version_declarations WHERE vid=?1 ORDER BY syntax_id")?
      .query_map([v],|r|Ok((stored_id_row(r,0,false)?,optional_stored_id_row(r,1,false)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,bool>(6)?,r.get::<_,Option<i64>>(7)?,r.get::<_,Option<bool>>(8)?,r.get::<_,i64>(9)?,r.get::<_,i64>(10)?,r.get::<_,i64>(11)?,r.get::<_,Option<i64>>(12)?,r.get::<_,Option<i64>>(13)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let scope = scope.clone();
    Ok(decoder(move |progress| {
        use crate::native_evidence::{Declaration, Header, Key, Parameter, Range};
        optional_hint_check(progress)?;
        let mut own_types: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in rows0 {
            optional_hint_check(progress)?;
            let (id, ord, value) = row;
            let entry = own_types.entry(id).or_default();
            contiguous(ord, entry.len(), "own signature")?;
            entry.push(value);
        }
        let mut ancestor_types: BTreeMap<(String, i64), Vec<String>> = BTreeMap::new();
        for row in rows1 {
            optional_hint_check(progress)?;
            let (id, ancestor, ord, value) = row;
            let entry = ancestor_types.entry((id, ancestor)).or_default();
            contiguous(ord, entry.len(), "ancestor signature")?;
            entry.push(value);
        }
        type Ancestor = (
            i64,
            String,
            Option<String>,
            i64,
            bool,
            Option<i64>,
            Option<bool>,
        );
        let mut ancestors: BTreeMap<String, Vec<Ancestor>> = BTreeMap::new();
        for row in rows2 {
            optional_hint_check(progress)?;
            let (id, ord, kind, name, sibling, present, count, variadic) = row;
            let entry = ancestors.entry(id).or_default();
            contiguous(ord, entry.len(), "ancestor")?;
            entry.push((ord, kind, name, sibling, present, count, variadic));
        }
        let mut headers: BTreeMap<String, Header> = BTreeMap::new();
        for row in rows3 {
            optional_hint_check(progress)?;
            let (id, kind, name, result_type) = row;
            selected_ensure!(
                headers
                    .insert(
                        id,
                        Header {
                            kind,
                            name,
                            modifiers: vec![],
                            type_parameters: vec![],
                            parameters: vec![],
                            result_type,
                            bases: vec![]
                        }
                    )
                    .is_none(),
                "duplicate native header"
            );
        }
        // Separate ordinal spaces for each (syntax_id,item_kind); don't collapse them.
        let mut item_ordinals: BTreeMap<(String, String), usize> = BTreeMap::new();
        for row in rows4 {
            optional_hint_check(progress)?;
            let (id, kind, ordinal, value) = row;
            let n = item_ordinals.entry((id.clone(), kind.clone())).or_default();
            contiguous(ordinal, *n, "header item")?;
            *n += 1;
            let h = headers
                .get_mut(&id)
                .ok_or_else(|| SelectedIntegrity("orphan native header item".into()))?;
            match kind.as_str() {
                "modifier" => h.modifiers.push(value),
                "typeParameter" => h.type_parameters.push(value),
                "base" => h.bases.push(value),
                _ => return Err(SelectedIntegrity("unknown header item".into()).into()),
            }
        }
        let mut param_ordinals: BTreeMap<String, usize> = BTreeMap::new();
        for row in rows5 {
            optional_hint_check(progress)?;
            let (id, ordinal, name, type_name, variadic) = row;
            let n = param_ordinals.entry(id.clone()).or_default();
            contiguous(ordinal, *n, "header parameter")?;
            *n += 1;
            headers
                .get_mut(&id)
                .ok_or_else(|| SelectedIntegrity("orphan native header parameter".into()))?
                .parameters
                .push(Parameter {
                    name,
                    type_name,
                    variadic,
                });
        }
        let proof = scope.proof_id();
        let mut result = Vec::new();
        let mut recorded_owners: BTreeMap<String, Option<String>> = BTreeMap::new();
        for row in rows6 {
            optional_hint_check(progress)?;
            let (
                id,
                owner,
                kind,
                role,
                name,
                lookup,
                present,
                count,
                variadic,
                ordinal,
                start,
                end,
                name_start,
                name_end,
            ) = row;
            let role = crate::native_evidence::DeclarationRole::from_stored(&role)
                .map_err(|error| SelectedIntegrity(error.to_string()))?;
            selected_ensure!(
                recorded_owners.insert(id.clone(), owner).is_none(),
                "duplicate native declaration"
            );
            let key = Key {
                kind: kind.clone(),
                name: name.clone(),
                signature: decode_signature(&mut own_types, &id, present, count, variadic)?,
                ordinal: usize::try_from(ordinal)
                    .map_err(|error| SelectedIntegrity(error.to_string()))?,
            };
            let mut a = Vec::new();
            for (ancestor_ord, kind, name, sibling, present, count, variadic) in
                ancestors.remove(&id).unwrap_or_default()
            {
                contiguous(ancestor_ord, a.len(), "ancestor")?;
                let sig_types = ancestor_types
                    .remove(&(id.clone(), ancestor_ord))
                    .unwrap_or_default();
                if !present {
                    selected_ensure!(sig_types.is_empty(), "orphan ancestor signature types");
                }
                let signature = if present {
                    Some(crate::native_evidence::Signature {
                        parameter_types: sig_types,
                        type_parameter_count: usize::try_from(count.ok_or_else(|| {
                            SelectedIntegrity("missing ancestor signature count".into())
                        })?)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?,
                        variadic: variadic.ok_or_else(|| {
                            SelectedIntegrity("missing ancestor signature variadic".into())
                        })?,
                    })
                } else {
                    None
                };
                a.push(Key {
                    kind,
                    name,
                    signature,
                    ordinal: usize::try_from(sibling)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?,
                });
            }
            let name_range = name_start
                .zip(name_end)
                .map(|(start, end)| -> Result<Range> {
                    Ok(Range {
                        start: usize::try_from(start)
                            .map_err(|error| SelectedIntegrity(error.to_string()))?,
                        end: usize::try_from(end)
                            .map_err(|error| SelectedIntegrity(error.to_string()))?,
                    })
                })
                .transpose()?;
            selected_ensure!(
                name_start.is_some() == name_end.is_some(),
                "half native name range"
            );
            result.push(Declaration {
                role,
                syntax_id: id.clone(),
                document: scope.key.clone(),
                revision_id: scope.native_revision_id.clone(),
                kind,
                name,
                lookup_key: lookup,
                ancestors: a,
                key,
                range: Range {
                    start: usize::try_from(start)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?,
                    end: usize::try_from(end)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?,
                },
                name_range,
                header: headers
                    .remove(&id)
                    .ok_or_else(|| SelectedIntegrity("missing native header".into()))?,
                provenance_id: proof.clone(),
            });
        }
        selected_ensure!(
            own_types.is_empty()
                && ancestor_types.is_empty()
                && ancestors.is_empty()
                && headers.is_empty(),
            "orphan native declaration children"
        );
        // Verify stored owner_syntax_id, a column absent from the public DTO. Writer maps parent
        // from serialized ancestor prefix and final Key; compare against complete decoded tree.
        let mut by_key: BTreeMap<(String, String), String> = BTreeMap::new();
        for d in &result {
            optional_hint_check(progress)?;
            let identity = (
                serde_json::to_string(&d.ancestors)?,
                serde_json::to_string(&d.key)?,
            );
            selected_ensure!(
                by_key.insert(identity, d.syntax_id.clone()).is_none(),
                "duplicate native owner key"
            );
        }
        for d in &result {
            optional_hint_check(progress)?;
            let expected = d
                .ancestors
                .last()
                .map(|last| -> Result<String> {
                    by_key
                        .get(&(
                            serde_json::to_string(&d.ancestors[..d.ancestors.len() - 1])?,
                            serde_json::to_string(last)?,
                        ))
                        .cloned()
                        .ok_or_else(|| {
                            anyhow::Error::from(SelectedIntegrity("missing native parent".into()))
                        })
                })
                .transpose()?;
            selected_ensure!(
                recorded_owners.get(&d.syntax_id) == Some(&expected),
                "native parent differs from ancestor key"
            );
        }
        Ok(result)
    }))
}
fn read_native_declarations_v8_inner(
    db: &Connection,
    scope: &V8NativeScope,
    lookup_key: Option<&str>,
    all_lookup_keys: bool,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<Vec<crate::native_evidence::Declaration>> {
    let mut result = fetch_native_declarations_v8(db, scope, progress)?(progress)?;
    if !all_lookup_keys {
        result.retain(|d| d.lookup_key.as_deref() == lookup_key);
    }
    Ok(result)
}

fn read_native_coverage_v8(
    db: &Connection,
    scope: &V8NativeScope,
) -> Result<crate::native_evidence::Coverage> {
    use crate::native_evidence::Coverage;
    let (requested,selected,state,diagnostic):(bool,bool,String,Option<String>)=
        db.query_row("SELECT coverage_requested,coverage_selected,coverage_state,coverage_diagnostic FROM revision_documents WHERE revision_id=?1 AND language=?2 AND path=?3 AND vid=?4",
            params![scope.revision_key,scope.key.language,scope.key.path,scope.vid],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let (mut supported_roles, mut observed_roles) = (vec![], vec![]);
    for row in db.prepare_cached("SELECT role_kind,role,ordinal FROM native_version_coverage_roles WHERE vid=?1 ORDER BY role_kind,ordinal")?
        .query_map([&scope.vid],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))? {
        let (kind,role,ordinal)=row?;
        let out=match kind.as_str(){"supported"=>&mut supported_roles,"observed"=>&mut observed_roles,_=>return Err(SelectedIntegrity("invalid native coverage role kind".into()).into())};
        contiguous(ordinal,out.len(),"coverage role")?;out.push(role);
    }
    Ok(Coverage {
        producer_id: scope.producer_id.clone(),
        language: scope.key.language.clone(),
        source_set_id: scope.key.source_set_id.clone(),
        document_path: scope.key.path.clone(),
        revision_id: scope.native_revision_id.clone(),
        requested,
        selected,
        state,
        supported_roles,
        observed_roles,
        diagnostic,
    })
}
// Reads all version rows, verifies all child edges, then selects the requested owner.
fn read_native_calls_v8(
    db: &Connection,
    scope: &V8NativeScope,
    owner: Option<&str>,
) -> Result<Vec<crate::native_evidence::Call>> {
    read_native_calls_v8_inner(db, scope, owner, None)
}
/// Read every row of one selected version; the returned decoder checks and
/// assembles them without the connection, so callers may decode in parallel.
fn fetch_native_calls_v8(
    db: &Connection,
    scope: &V8NativeScope,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<impl Decode<Vec<crate::native_evidence::Call>> + use<>> {
    optional_hint_check(progress)?;
    let version = &scope.vid;
    let rows0 = db
        .prepare_cached("SELECT syntax_id FROM native_version_declarations WHERE vid=?1")?
        .query_map([version], |r| stored_id_row(r, 0, false))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows1 = db
        .prepare_cached(
            "SELECT id,owner_syntax_id FROM native_version_control_regions WHERE vid=?1",
        )?
        .query_map([version], |r| {
            Ok((stored_id_row(r, 0, true)?, stored_id_row(r, 1, false)?))
        })?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows2 = db.prepare_cached("SELECT id,owner_syntax_id,ordinal,start_byte,end_byte,callee_start,callee_end,spelling,language,callee_lookup_key FROM native_version_calls WHERE vid=?1 ORDER BY owner_syntax_id,ordinal")?
        .query_map([version],|r|Ok((stored_id_row(r,0,true)?,stored_id_row(r,1,false)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,Option<i64>>(5)?,r.get::<_,Option<i64>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,String>(8)?,r.get::<_,Option<String>>(9)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows3 = db.prepare_cached("SELECT call_id,region_id,ordinal FROM native_version_call_regions WHERE vid=?1 ORDER BY call_id,ordinal")?
        .query_map([version],|r|Ok((stored_id_row(r,0,true)?,stored_id_row(r,1,true)?,r.get::<_,i64>(2)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let scope = scope.clone();
    Ok(decoder(move |progress| {
        optional_hint_check(progress)?;
        use crate::native_evidence::{Call, Range};
        let mut known = BTreeSet::new();
        for row in rows0 {
            optional_hint_check(progress)?;
            known.insert(row);
        }
        let mut region_owner = BTreeMap::new();
        for row in rows1 {
            optional_hint_check(progress)?;
            let (id, owner) = row;
            region_owner.insert(id, owner);
        }
        let proof = scope.proof_id();
        let mut calls: Vec<Call> = vec![];
        let mut indexes: BTreeMap<String, usize> = BTreeMap::new();
        let mut ordinals: BTreeMap<String, usize> = BTreeMap::new();
        for row in rows2 {
            optional_hint_check(progress)?;
            let (
                id,
                call_owner,
                ordinal,
                start,
                end,
                callee_start,
                callee_end,
                spelling,
                language,
                callee_lookup_key,
            ) = row;
            selected_ensure!(
                language == scope.key.language,
                "native call document mismatch"
            );
            selected_ensure!(
                callee_lookup_key.is_none() || callee_start.is_some() && spelling.is_some(),
                "invalid native call lookup key"
            );
            selected_ensure!(known.contains(&call_owner), "orphan native call owner");
            let n = ordinals.entry(call_owner.clone()).or_default();
            contiguous(ordinal, *n, "call")?;
            *n += 1;
            selected_ensure!(
                callee_start.is_some() == callee_end.is_some(),
                "half callee range"
            );
            let callee_range = callee_start
                .zip(callee_end)
                .map(|(start, end)| -> Result<Range> {
                    Ok(Range {
                        start: usize::try_from(start)
                            .map_err(|error| SelectedIntegrity(error.to_string()))?,
                        end: usize::try_from(end)
                            .map_err(|error| SelectedIntegrity(error.to_string()))?,
                    })
                })
                .transpose()?;
            selected_ensure!(
                indexes.insert(id.clone(), calls.len()).is_none(),
                "duplicate native call ID"
            );
            calls.push(Call {
                id,
                owner_syntax_id: call_owner,
                ordinal: usize::try_from(ordinal)
                    .map_err(|error| SelectedIntegrity(error.to_string()))?,
                document: scope.key.clone(),
                revision_id: scope.native_revision_id.clone(),
                range: Range {
                    start: usize::try_from(start)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?,
                    end: usize::try_from(end)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?,
                },
                callee_range,
                spelling,
                region_ids: vec![],
                provenance_id: proof.clone(),
            });
        }
        for row in rows3 {
            optional_hint_check(progress)?;
            let (call_id, region_id, ordinal) = row;
            let call = &mut calls[*indexes
                .get(&call_id)
                .ok_or_else(|| SelectedIntegrity("orphan native call region".into()))?];
            selected_ensure!(
                region_owner.get(&region_id) == Some(&call.owner_syntax_id),
                "cross-owner native call region"
            );
            contiguous(ordinal, call.region_ids.len(), "call region")?;
            call.region_ids.push(region_id);
        }
        Ok(calls)
    }))
}
fn read_native_calls_v8_inner(
    db: &Connection,
    scope: &V8NativeScope,
    owner: Option<&str>,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<Vec<crate::native_evidence::Call>> {
    let calls = fetch_native_calls_v8(db, scope, progress)?(progress)?;
    if owner.is_none() {
        optional_hint_check(progress)?;
        return Ok(calls);
    }
    let mut selected = Vec::new();
    for call in calls {
        optional_hint_check(progress)?;
        if owner.is_some_and(|id| call.owner_syntax_id == id) {
            selected.push(call);
        }
    }
    Ok(selected)
}
fn read_native_control_regions_v8(
    db: &Connection,
    scope: &V8NativeScope,
    owner: Option<&str>,
) -> Result<Vec<crate::native_evidence::ControlRegion>> {
    read_native_control_regions_v8_inner(db, scope, owner, None)
}
/// Read every row of one selected version; the returned decoder checks and
/// assembles them without the connection, so callers may decode in parallel.
fn fetch_native_control_regions_v8(
    db: &Connection,
    scope: &V8NativeScope,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<impl Decode<Vec<crate::native_evidence::ControlRegion>> + use<>> {
    optional_hint_check(progress)?;
    let rows0 = db
        .prepare_cached("SELECT syntax_id FROM native_version_declarations WHERE vid=?1")?
        .query_map([&scope.vid], |r| stored_id_row(r, 0, false))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let rows1 = db.prepare_cached("SELECT id,owner_syntax_id,ordinal,kind,start_byte,end_byte,parent_id,arm FROM native_version_control_regions WHERE vid=?1 ORDER BY owner_syntax_id,ordinal")?
        .query_map([&scope.vid],|r|Ok((stored_id_row(r,0,true)?,stored_id_row(r,1,false)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?,optional_stored_id_row(r,6,true)?,r.get::<_,Option<String>>(7)?)))?
        .map(|row| -> Result<_> {
            optional_hint_check(progress)?;
            Ok(row?)
        })
        .collect::<Result<Vec<_>>>()?;
    let scope = scope.clone();
    Ok(decoder(move |progress| {
        optional_hint_check(progress)?;
        use crate::native_evidence::{ControlRegion, Range};
        let mut known = BTreeSet::new();
        for row in rows0 {
            optional_hint_check(progress)?;
            known.insert(row);
        }
        let proof = scope.proof_id();
        let mut regions = Vec::<ControlRegion>::new();
        let mut indexes = BTreeMap::<String, usize>::new();
        let mut ordinals = BTreeMap::<String, usize>::new();
        for row in rows1 {
            optional_hint_check(progress)?;
            let (id, region_owner, ordinal, kind, start, end, parent_id, arm) = row;
            selected_ensure!(known.contains(&region_owner), "orphan native region owner");
            let n = ordinals.entry(region_owner.clone()).or_default();
            contiguous(ordinal, *n, "region")?;
            *n += 1;
            selected_ensure!(
                indexes.insert(id.clone(), regions.len()).is_none(),
                "duplicate native region ID"
            );
            regions.push(ControlRegion {
                id,
                owner_syntax_id: region_owner,
                ordinal: usize::try_from(ordinal)
                    .map_err(|error| SelectedIntegrity(error.to_string()))?,
                document: scope.key.clone(),
                revision_id: scope.native_revision_id.clone(),
                kind,
                range: Range {
                    start: usize::try_from(start)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?,
                    end: usize::try_from(end)
                        .map_err(|error| SelectedIntegrity(error.to_string()))?,
                },
                parent_id,
                arm,
                provenance_id: proof.clone(),
            });
        }
        for region in &regions {
            optional_hint_check(progress)?;
            if let Some(parent_id) = &region.parent_id {
                let parent = &regions[*indexes
                    .get(parent_id)
                    .ok_or_else(|| SelectedIntegrity("missing native parent region".into()))?];
                selected_ensure!(
                    region.owner_syntax_id == parent.owner_syntax_id
                        && parent.range.start <= region.range.start
                        && region.range.end <= parent.range.end,
                    "invalid native parent region"
                );
            }
        }
        Ok(regions)
    }))
}
fn read_native_control_regions_v8_inner(
    db: &Connection,
    scope: &V8NativeScope,
    owner: Option<&str>,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<Vec<crate::native_evidence::ControlRegion>> {
    let regions = fetch_native_control_regions_v8(db, scope, progress)?(progress)?;
    if owner.is_none() {
        optional_hint_check(progress)?;
        return Ok(regions);
    }
    let mut selected = Vec::new();
    for region in regions {
        optional_hint_check(progress)?;
        if owner.is_some_and(|id| region.owner_syntax_id == id) {
            selected.push(region);
        }
    }
    Ok(selected)
}

fn read_native_references(
    db: &Connection,
    scope: &V8NativeScope,
    owner: Option<&str>,
) -> Result<Vec<crate::native_evidence::Reference>> {
    use crate::native_evidence::{Range, Reference};
    let mut known = BTreeSet::new();
    for row in db
        .prepare("SELECT syntax_id FROM native_version_declarations WHERE vid=?1")?
        .query_map([scope.vid], |r| stored_id_row(r, 0, false))?
    {
        known.insert(row?);
    }
    let mut references = vec![];
    let mut ordinals = BTreeMap::<String, usize>::new();
    // One document's references share one proof: digest it once, not per row.
    let proof = scope.proof_id();
    for row in db.prepare("SELECT id,owner_syntax_id,ordinal,start_byte,end_byte,spelling,COALESCE(lookup_key,spelling) FROM native_version_references WHERE vid=?1 ORDER BY owner_syntax_id,ordinal")?
        .query_map([scope.vid], |r| Ok((stored_id_row(r, 0, true)?, stored_id_row(r, 1, false)?,
            r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?, r.get::<_, String>(6)?)))? {
        let (id, reference_owner, ordinal, start, end, spelling, lookup_key) = row?;
        selected_ensure!(known.contains(&reference_owner), "orphan native reference owner");
        selected_ensure!(start >= 0 && end > start, "invalid native reference range");
        let n = ordinals.entry(reference_owner.clone()).or_default();
        contiguous(ordinal, *n, "reference")?;
        *n += 1;
        references.push(Reference {
            id, owner_syntax_id: reference_owner, ordinal: ordinal as usize,
            document: scope.key.clone(), revision_id: scope.native_revision_id.clone(),
            range: Range { start: start as usize, end: end as usize }, spelling, lookup_key,
            provenance_id: proof.clone(),
        });
    }
    if let Some(owner) = owner {
        references.retain(|reference| reference.owner_syntax_id == owner);
    }
    Ok(references)
}

fn read_native_capability_report(
    db: &Connection,
    scope: &V8NativeScope,
) -> Result<crate::native_evidence::CapabilityReport> {
    use crate::native_evidence::{
        CapabilityReport, MeasurementGap, MeasurementKind, MeasurementSupport, Range,
    };
    let mut families = vec![];
    let mut kinds = BTreeMap::new();
    for row in db.prepare("SELECT kind,available,diagnostic,ordinal FROM native_version_capability_families WHERE vid=?1 ORDER BY ordinal")?
        .query_map([scope.vid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<String>>(2)?, r.get::<_, i64>(3)?)))? {
        let (kind, available, diagnostic, ordinal) = row?;
        let decoded: MeasurementKind = serde_json::from_value(serde_json::Value::String(kind.clone()))
            .context(SelectedIntegrity("invalid native capability kind".into()))?;
        selected_ensure!(matches!(available, 0 | 1) &&
            (if available == 1 { diagnostic.is_none() } else { diagnostic.as_ref().is_some_and(|d| !d.is_empty()) }),
            "invalid native capability availability");
        contiguous(ordinal, families.len(), "capability family")?;
        selected_ensure!(kinds.insert(kind, families.len()).is_none(), "duplicate native capability family");
        families.push(MeasurementSupport { kind: decoded, available: available == 1, diagnostic, gaps: vec![] });
    }
    selected_ensure!(families.len() == 4, "incomplete native capability report");
    for row in db.prepare("SELECT kind,ordinal,start_byte,end_byte,diagnostic FROM native_version_capability_gaps WHERE vid=?1 ORDER BY kind,ordinal")?
        .query_map([scope.vid], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?, r.get::<_, String>(4)?)))? {
        let (kind, ordinal, start, end, diagnostic) = row?;
        let family = &mut families[*kinds.get(&kind).ok_or_else(|| SelectedIntegrity("orphan native capability gap".into()))?];
        contiguous(ordinal, family.gaps.len(), "capability gap")?;
        selected_ensure!(family.available && start >= 0 && end > start && !diagnostic.is_empty(), "invalid native capability gap");
        family.gaps.push(MeasurementGap { range: Range { start: start as usize, end: end as usize }, diagnostic });
    }
    Ok(CapabilityReport {
        document: scope.key.clone(),
        revision_id: scope.native_revision_id.clone(),
        families,
    })
}

impl Store {
    /// Wake `waker` after every enqueue COMMIT on this checkout's queue, by any
    /// Store instance in this process. Held weakly: dropping it unregisters.
    pub fn register_queue_waker(&self, waker: &Arc<dyn Fn() + Send + Sync>) {
        self.publication_gate
            .queue_wakers
            .lock()
            .unwrap()
            .push(Arc::downgrade(waker));
    }

    pub(crate) fn wake_queue_workers(&self) {
        self.publication_gate.wake_queue();
    }

    /// Retirement's linearization point: mark this queue's runtime closing,
    /// only if no enqueue is in flight (one atomic step).
    pub(crate) fn try_begin_closing(&self) -> bool {
        self.publication_gate
            .enqueue_word
            .compare_exchange(
                0,
                CLOSING,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }

    /// The closing runtime has retired; a fresh one may activate.
    pub(crate) fn end_closing(&self) {
        self.publication_gate
            .enqueue_word
            .fetch_and(!CLOSING, std::sync::atomic::Ordering::AcqRel);
    }

    /// Count an enqueue as in flight until the returned guard drops.
    pub(crate) fn enqueue_in_flight(&self) -> EnqueueInFlight {
        self.publication_gate
            .enqueue_word
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        EnqueueInFlight(Arc::clone(&self.publication_gate))
    }

    /// Pause the next enqueue on this queue after its COMMIT, before its wake.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_after_enqueue_commit_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        *self
            .publication_gate
            .test_after_enqueue_commit
            .lock()
            .unwrap() = Some(Box::new(hook));
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn run_after_enqueue_commit_hook(&self) {
        let hook = self
            .publication_gate
            .test_after_enqueue_commit
            .lock()
            .unwrap()
            .take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Signal waiters after a confirmed terminal request COMMIT.
    pub(crate) fn notify_terminal_waiters(&self) {
        self.publication_gate.notify_terminal();
        crate::index_coordinator::diagnostic_marker(
            "-",
            "terminal_notify",
            &format!("generation={}", self.terminal_generation()),
        );
    }

    /// The terminal/queue-loss wake generation to record before re-reading a row.
    pub fn terminal_generation(&self) -> u64 {
        *self.publication_gate.terminal_commits.lock().unwrap()
    }

    /// Block until a terminal or queue-loss wake after `seen`; then re-read the row.
    pub fn wait_terminal_change(&self, seen: u64) {
        let gate = &self.publication_gate;
        let commits = gate.terminal_commits.lock().unwrap();
        #[cfg(feature = "test-support")]
        gate.test_before_terminal_wait.run();
        drop(
            gate.terminal_changed
                .wait_while(commits, |current| *current == seen)
                .unwrap(),
        );
    }

    /// The waiting count is registered before waiting, giving new publications
    /// priority over the next maintenance unit. The permit is owned across ACK.
    pub fn enter_publication(
        &self,
        cancel: &CancelFlag,
        max_wait: Duration,
    ) -> Result<PublishPermit> {
        let started = Instant::now();
        let deadline = started + max_wait;
        let gate = &self.publication_gate;
        let mut state = gate.state.lock().unwrap();
        let mut wait_reason = PublicationWaitReason::None;
        state.waiting += 1;
        gate.publish_waiting(&state);
        loop {
            if cancel.load(Ordering::Acquire) {
                state.waiting -= 1;
                gate.publish_waiting(&state);
                gate.changed.notify_all();
                anyhow::bail!("index publication cancelled");
            }
            if !state.active {
                state.waiting -= 1;
                gate.publish_waiting(&state);
                state.active = true;
                state.maintenance = false;
                return Ok(PublishPermit {
                    gate: Arc::clone(gate),
                    waited_for: started.elapsed(),
                    wait_reason,
                });
            }
            if wait_reason == PublicationWaitReason::None {
                wait_reason = if state.maintenance {
                    PublicationWaitReason::Maintenance
                } else {
                    PublicationWaitReason::Publisher
                };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.waiting -= 1;
                gate.publish_waiting(&state);
                gate.changed.notify_all();
                return Err(SqliteContention(()).into());
            }
            // Woken when a permit is released (#138 item 14); no caller cancels
            // a gate wait from outside, so cancellation is checked per wake.
            state = gate.changed.wait_timeout(state, remaining).unwrap().0;
        }
    }

    pub fn maintenance_try_enter(&self) -> Option<MaintenancePermit> {
        let gate = &self.publication_gate;
        // An O(1) section: wait for the lock rather than reading a briefly
        // held one as busy (#138 item 13).
        let mut state = gate.state.lock().unwrap();
        if state.active || state.waiting != 0 {
            return None;
        }
        state.active = true;
        state.maintenance = true;
        Some(MaintenancePermit {
            gate: Arc::clone(gate),
        })
    }

    fn maintenance_priority(
        &self,
        permit: &MaintenancePermit,
        probe: &QueueProbeAdmission,
        priority: &mut impl FnMut() -> bool,
    ) -> bool {
        let waiting = self.publication_gate.state.lock().unwrap().waiting;
        Arc::ptr_eq(&self.publication_gate, &permit.gate)
            && waiting == 0
            && probe.check() == MaintenanceQueueState::Clear
            && std::panic::catch_unwind(std::panic::AssertUnwindSafe(priority)).unwrap_or(false)
    }
    pub fn open(
        roots: topology::TopologyRoots,
        identity: topology::WorkspaceIdentity,
    ) -> Result<Self> {
        Self::open_with_stage_hook(roots, identity, |_| Ok(()))
    }
    fn open_with_stage_hook(
        roots: topology::TopologyRoots,
        identity: topology::WorkspaceIdentity,
        before_publish: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<Self> {
        identity.verify()?;
        // Activation alone provisions the directory and its sibling use lock.
        // Release SH before recovery may need EX admission.
        drop(roots.index_use(&identity)?);
        let store = Self::unopened(roots, identity)?;
        if !index_path_present(&store.roots.index_db(&store.identity))? {
            let leader = store.roots.writer(store.identity.clone())?;
            let initialized = store.initialize(&leader, before_publish);
            drop(leader);
            if let Some(stage) = store.aborted_staged_index.lock().unwrap().take()
                && let Err(cleanup) = store.cleanup_failed_staged_index(stage, None)
            {
                return Err(match initialized {
                    Err(original) => {
                        cleanup.context(format!("{original}: failed stage cleanup refused"))
                    }
                    Ok(()) => cleanup,
                });
            }
            initialized?;
        }
        let admit = || -> Result<()> {
            let mut db = store.cache()?;
            let tx = storage_result(db.transaction())?;
            let _ = store.recovery_baseline_full(&tx)?;
            Ok(())
        };
        // Recover a killed publication's hot journal before the first admission:
        // a torn page-1 header must be restored by rollback, never classified
        // as corruption by the header check that admission runs first.
        if hot_index_journal_present(&store.roots.index_db(&store.identity))? {
            store.recover_hot_index_journal()?;
        }
        let admission = match admit() {
            // A daemon killed mid-COMMIT leaves a hot journal that a read-only
            // connection cannot roll back. Recover it, then admit normally.
            Err(error) if error.to_string() == HOT_INDEX_JOURNAL => {
                store.recover_hot_index_journal()?;
                admit()
            }
            other => other,
        };
        match admission {
            Ok(()) => Ok(store),
            Err(error) if recovery_class(&error) == RecoveryClass::RecreatePending => {
                if let Some(obsolete) = error.downcast_ref::<ObsoleteIndexFormat>() {
                    *store.obsolete_format_marker.lock().unwrap() = Some(obsolete.clone());
                }
                store.mark_recovery(RecoveryDisposition::RecreatePending);
                Ok(store)
            }
            Err(error) if recovery_class(&error) == RecoveryClass::Rebuild => {
                if let Some(obsolete) = error.downcast_ref::<ObsoleteIndexFormat>() {
                    *store.obsolete_format_marker.lock().unwrap() = Some(obsolete.clone());
                }
                store.mark_recovery(RecoveryDisposition::Rebuild);
                Ok(store)
            }
            Err(error) if error.to_string() == "root_changed: index root identity mismatch" => {
                store.mark_recovery(RecoveryDisposition::RootReplaced);
                Ok(store)
            }
            Err(error) => Err(error),
        }
    }
    fn unopened(
        roots: topology::TopologyRoots,
        identity: topology::WorkspaceIdentity,
    ) -> Result<Self> {
        Ok(Self {
            workspace_root: identity
                .root
                .to_str()
                .context("workspace path is not UTF-8")?
                .to_owned(),
            publication_gate: publication_gate(format!(
                "{}:{}:{}",
                identity.root.display(),
                identity.device,
                identity.inode
            )),
            roots,
            identity: Arc::new(identity),
            maintenance_busy_attempts: Arc::new(AtomicU64::new(0)),
            #[cfg(feature = "test-support")]
            mcp_read_busy_attempts: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            recovery_required: Arc::new(AtomicBool::new(false)),
            recovery_disposition: Arc::new(AtomicU8::new(RecoveryDisposition::Ready as u8)),
            obsolete_format_marker: Arc::new(Mutex::new(None)),
            pending_request_completion: Arc::new(Mutex::new(None)),
            request_file_witness: Arc::new(Mutex::new(None)),
            aborted_staged_index: Arc::new(Mutex::new(None)),
            writer_counters: Arc::new(Mutex::new(None)),
            reconciled_writer: Arc::new(Mutex::new(None)),
            metrics_memo: Arc::new(Mutex::new(crate::metrics::MetricsMemo::new(
                64 * 1024 * 1024,
            ))),
            admitted: None,
            published_admission: Arc::default(),
            retention_clock: Arc::new(Mutex::new(RetentionClock {
                origin_wall: publication_second()?,
                origin_mono: 0,
                started: Instant::now(),
                injected: None,
            })),
            #[cfg(feature = "test-support")]
            test_after_claim_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(test)]
            test_before_request_terminal_commit_hook: Arc::new(TestOneShotHook::default()),
            maintenance_before_writer_hook: Arc::new(TestOneShotHook::default()),
            maintenance_after_first_delete_hook: Arc::new(TestOneShotHook::default()),
            publication_before_commit_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(feature = "test-support")]
            publication_before_semantic_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(test)]
            test_stream_before_preflight_hook: Arc::new(Mutex::new(None)),
            publication_cache_kib: Arc::new(std::sync::atomic::AtomicI64::new(
                PUBLICATION_CACHE_KIB,
            )),
            writer_before_metadata_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(test)]
            test_queue_before_shared_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(test)]
            test_metrics_document_hook: Arc::new(Mutex::new(None)),
            #[cfg(feature = "test-support")]
            test_queue_after_root_verify_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(test)]
            test_queue_select_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(test)]
            test_native_spool_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(any(test, feature = "test-support"))]
            test_queue_terminal_notify: Arc::new(Mutex::new(None)),
            #[cfg(feature = "test-support")]
            test_ambiguous_completion_once: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            test_exclusive_recovery_hook: Arc::new(TestOneShotHook::default()),
            #[cfg(test)]
            test_queue_finish_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            test_queue_post_commit_failures: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            #[cfg(test)]
            test_publish_commit_busy_once: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            test_publish_commit_typed_busy_once: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            test_publish_post_commit_busy_once: Arc::new(AtomicBool::new(false)),
        })
    }
    /// Read only bounded index identity metadata for browser discovery. This
    /// existing-only probe shares the process-wide SQLite witness with readers
    /// and writers. It does not run recovery or inspect evidence rows.
    pub fn browser_index_root_existing(
        roots: &topology::TopologyRoots,
        key: &str,
    ) -> std::result::Result<String, &'static str> {
        use sha2::Digest;
        if key.len() != 64
            || !key
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err("unavailable");
        }
        let parent = roots.cache.join("indexes");
        let directory = parent.join(key);
        for path in [&roots.cache, &parent, &directory] {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
                _ => return Err("unavailable"),
            }
        }
        let lock = parent.join(format!("{key}.lock"));
        let _use_guard = topology::UseGuard::acquire_existing_readonly(&lock).map_err(|error| {
            if error.is::<topology::StorageBusy>() {
                "storage_busy"
            } else {
                "unavailable"
            }
        })?;
        let index = directory.join("index.db");
        let observed = (|| -> std::result::Result<String, &'static str> {
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = directory.join(format!("index.db{suffix}"));
                match std::fs::symlink_metadata(sidecar) {
                    Ok(_) => return Err("storage_busy"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err("unavailable"),
                }
            }
            // Both the header witness and SQLite descriptor remain registered for
            // this inode throughout the read, including concurrent active Stores.
            let db = open_index_marker_probe(&index, false, Duration::from_millis(20)).map_err(
                |error| {
                    if error.is::<topology::StorageBusy>()
                        || error.to_string().starts_with("storage_busy")
                    {
                        "storage_busy"
                    } else if error.to_string().starts_with("incompatible_index")
                        || error.is::<ExceptionalIndexFormat>()
                    {
                        "corrupt"
                    } else {
                        "unavailable"
                    }
                },
            )?;
            let marker = read_index_format_marker(&db).map_err(|error| {
                if error.is::<topology::StorageBusy>() {
                    "storage_busy"
                } else {
                    "corrupt"
                }
            })?;
            let version: u32 =
                storage_result(db.pragma_query_value(None, "user_version", |row| row.get(0)))
                    .map_err(|error| {
                        if error.is::<topology::StorageBusy>() {
                            "storage_busy"
                        } else {
                            "unavailable"
                        }
                    })?;
            if marker.is_obsolete()
                || version != DATABASE_SCHEMA_VERSION
                || marker.schema_version != i64::from(DATABASE_SCHEMA_VERSION)
            {
                return Err("corrupt");
            }
            let (length, spelling): (i64, Option<String>) = storage_result(db.query_row(
            "SELECT length(CAST(root_spelling AS BLOB)), CASE WHEN length(CAST(root_spelling AS BLOB))<=8192 THEN root_spelling END FROM index_metadata WHERE singleton=1 LIMIT 2",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        )).map_err(|error| if error.is::<topology::StorageBusy>() { "storage_busy" } else { "corrupt" })?;
            if !(1..=8192).contains(&length) {
                return Err("corrupt");
            }
            let spelling = spelling.ok_or("corrupt")?;
            if !Path::new(&spelling).is_absolute()
                || hex::encode(sha2::Sha256::digest(spelling.as_bytes())) != key
            {
                return Err("corrupt");
            }
            Ok(spelling)
        })();
        drop(_use_guard);
        // An active checkout holds SH on the use lock between its SQLite
        // operations. Only an idle EX proof permits dropping its retained fd;
        // a concurrent active Store or live connection keeps its own witness.
        if let Ok(_exclusive) = topology::UseGuard::acquire_existing_readonly_exclusive(&lock) {
            drop(SqliteWitnessRetirement(vec![index]));
        }
        observed
    }

    /// Observe only an already-published index. In particular, this path may
    /// not create HOME/cache, a Git identity marker, an index or a use lock.
    pub fn status_existing_readonly(
        roots: topology::TopologyRoots,
        identity: topology::WorkspaceIdentity,
    ) -> Result<IndexStatus> {
        let identity = identity.attach_existing_marker_readonly()?;
        roots.reject_root_overlap(&identity)?;
        let index = roots.index_db(&identity);
        ensure!(
            index_path_present(&index)?,
            "index_not_ready: no published index"
        );
        let store = Self::unopened(roots, identity)?;
        let guard = store.roots.index_use_existing_readonly(&store.identity)?;
        let observed = (|| {
            // Even a read-only SQLite open can try to recover a hot journal or
            // create WAL shared-memory sidecars. Status explicitly reports busy
            // instead of recovering, creating sidecars or ignoring live WAL.
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = index.with_file_name(format!(
                    "{}{}",
                    index
                        .file_name()
                        .context("index filename missing")?
                        .to_string_lossy(),
                    suffix
                ));
                match std::fs::symlink_metadata(sidecar) {
                    Ok(_) => anyhow::bail!("storage_busy: index journal sidecar present"),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let db = open_index(&index, false).map_err(|error| {
                if recovery_class(&error) == RecoveryClass::RecreatePending {
                    anyhow::anyhow!(
                        "recovery_required: exceptional index format; run explicit trellis index"
                    )
                } else {
                    error
                }
            })?;
            // Status does not replay saved options, but malformed/relative saved
            // inputs must still refuse rather than look like a healthy publication.
            let (revision, raw_options): (i64, Option<String>) = db.query_row(
                "SELECT index_revision,reconcile_options FROM index_metadata WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            ensure!(revision > 0, "index_not_ready: no published revision");
            let raw_options =
                raw_options.context("incompatible_index: missing reconcile options")?;
            let options: crate::indexer::ReconcileOptions = serde_json::from_str(&raw_options)
                .context(SelectedIntegrity(
                    "incompatible_index: stored JSON decode failed".into(),
                ))
                .context("incompatible_index: invalid reconcile options")?;
            ensure!(
                json(&options)? == raw_options && options.version == 2,
                "incompatible_index: unsupported reconcile options"
            );
            let status = store.read_status(&db)?;
            store.identity.verify_readonly()?;
            guard.verify()?;
            Ok(status)
        })();
        drop(guard);
        let lock = store
            .roots
            .cache
            .join("indexes")
            .join(format!("{}.lock", store.identity.root_key));
        if let Ok(_exclusive) = topology::UseGuard::acquire_existing_readonly_exclusive(&lock) {
            drop(SqliteWitnessRetirement(vec![index]));
        }
        observed
    }
    /// Isolated roots for integration fixtures; production startup calls `open` with ProjectDirs.
    #[cfg(test)]
    pub(crate) fn fail_next_live_publish_commit_busy(&self) {
        assert!(
            !self
                .test_publish_commit_typed_busy_once
                .load(Ordering::Acquire),
            "only one live pre-commit BUSY fixture may be armed"
        );
        self.test_publish_commit_busy_once
            .store(true, Ordering::Release);
    }
    /// Direct typed BUSY at the same live pre-COMMIT point. Do not arm this
    /// alongside the legacy plain-string fixture in one Store.
    #[cfg(test)]
    pub(crate) fn fail_next_live_publish_commit_typed_busy(&self) {
        assert!(
            !self.test_publish_commit_busy_once.load(Ordering::Acquire)
                && !self
                    .test_publish_commit_typed_busy_once
                    .load(Ordering::Acquire),
            "only one live pre-commit BUSY fixture may be armed"
        );
        self.test_publish_commit_typed_busy_once
            .store(true, Ordering::Release);
    }
    #[cfg(test)]
    pub(crate) fn fail_next_live_publish_post_commit_busy(&self) {
        self.test_publish_post_commit_busy_once
            .store(true, Ordering::Release);
    }
    pub fn open_for_tests(state: &Path, workspace: &Path) -> Result<Self> {
        let identity = topology::WorkspaceIdentity::discover(Some(workspace), workspace)?;
        let roots =
            topology::TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"));
        Self::open(roots, identity)
    }
    /// Fixture barrier after building a staged index, before its validation and publication.
    pub fn open_for_tests_with_index_stage_hook(
        state: &Path,
        workspace: &Path,
        before_publish: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<Self> {
        let identity = topology::WorkspaceIdentity::discover(Some(workspace), workspace)?;
        let roots =
            topology::TopologyRoots::isolated_for_tests(state.join("cache"), state.join("data"));
        Self::open_with_stage_hook(roots, identity, before_publish)
    }
    /// Build a private schema-10 bootstrap only; the caller must validate and publish it.
    /// The returned guard unlinks only its own stage inode if it is not published.
    fn cleanup_failed_staged_index(
        &self,
        staged: StagedIndex,
        exclusive_leader: Option<&topology::WriterGuard>,
    ) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            !staged.published,
            "unsafe_index: cannot discard published stage"
        );
        let path = staged.path.clone();
        let metadata = staged.file.metadata()?;
        let inode = (metadata.dev(), metadata.ino());
        let use_path = self.roots.index_use_lock(&self.identity);
        // Never unlink until exclusive admission; inability to prove this
        // leaves a named temporary file rather than leaked deleted-file space.
        let acquired = if exclusive_leader.is_none() {
            Some(self.roots.index_use_exclusive_existing(&self.identity)?)
        } else {
            None
        };
        let guard = match exclusive_leader {
            Some(leader) => leader.exclusive_use_guard(&use_path)?,
            None => acquired.as_ref().expect("verified exclusive guard"),
        };
        match std::fs::symlink_metadata(&path) {
            Ok(named) if (named.dev(), named.ino()) == inode => staged.verify_path()?,
            Ok(_) => return Err(ForeignStagedIndex.into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => ensure!(
                metadata.nlink() == 0,
                "unsafe_index: failed stage inode location not proved"
            ),
            Err(error) => return Err(error.into()),
        }
        {
            let registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
            ensure!(
                registry
                    .live
                    .get(&(path.clone(), inode.0, inode.1))
                    .copied()
                    .unwrap_or(0)
                    == 0,
                "storage_busy: failed stage still has live SQLite connections"
            );
            let files = registry
                .by_path
                .get(&path)
                .context("unsafe_index: failed stage witness missing")?;
            ensure!(
                files.len() == 1 && Arc::strong_count(&staged.file) == 2,
                "storage_busy: failed stage witness still borrowed"
            );
            match std::fs::symlink_metadata(&path) {
                Ok(named) if (named.dev(), named.ino()) == inode => {
                    std::fs::remove_file(&path)?;
                }
                Ok(_) => return Err(ForeignStagedIndex.into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        drop(staged); // release only the stage's Arc, never a live SQLite fd
        release_deleted_sqlite_witness_kind(&path, inode, guard, true)
    }
    fn create_staged_index(
        &self,
        leader: &topology::WriterGuard,
        under_exclusive: bool,
    ) -> Result<StagedIndex> {
        use rusqlite::OpenFlags;
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        let path = self.roots.index_db(&self.identity);
        let staged_path = path.with_file_name(format!("index.db.tmp-{}", uuid::Uuid::new_v4()));
        let file = create_retained_staged_sqlite_file(&staged_path)?;
        let staged = StagedIndex {
            path: staged_path,
            file,
            published: false,
        };
        let creation = (|| -> Result<()> {
            let db = protected_sqlite_open(
                &staged.path,
                OpenFlags::SQLITE_OPEN_READ_WRITE
                    | OpenFlags::SQLITE_OPEN_CREATE
                    | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            db.busy_timeout(Duration::ZERO)?;
            db.pragma_update(None, "journal_mode", "DELETE")?;
            db.pragma_update(None, "synchronous", "FULL")?;
            db.execute_batch("BEGIN IMMEDIATE")?;
            let result = (|| -> Result<()> {
                db.execute_batch(CACHE_SCHEMA_V29)?;
                let age = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                ensure!(age <= 9_007_199_254_740_991, "invalid_open_age");
                db.execute(
                    "INSERT INTO index_metadata VALUES(1,?9,?1,?2,?3,?4,?5,0,?6,'',?7,?8,NULL)",
                    params![
                        EXTRACTOR_VERSION,
                        self.workspace_root,
                        self.identity.device.to_string(),
                        self.identity.inode.to_string(),
                        uuid::Uuid::new_v4().to_string(),
                        age as i64,
                        json(&IndexStats::default())?,
                        json(&Vec::<Diagnostic>::new())?,
                        DATABASE_SCHEMA_VERSION
                    ],
                )?;
                db.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)?;
                db.execute_batch("COMMIT")?;
                Ok(())
            })();
            if result.is_err() {
                let _ = db.execute_batch("ROLLBACK");
            }
            result?;
            drop(db);
            Ok(())
        })();
        match creation {
            Ok(()) => Ok(staged),
            Err(error) => {
                if under_exclusive {
                    self.cleanup_failed_staged_index(staged, Some(leader))?;
                } else {
                    *self.aborted_staged_index.lock().unwrap() = Some(staged);
                }
                Err(error)
            }
        }
    }
    fn validate_replacement_index(
        &self,
        path: &Path,
        stage: &StagedIndex,
        leader: &topology::WriterGuard,
        pin: IndexPin,
    ) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let named = std::fs::symlink_metadata(path)?;
        let held = stage.file.metadata()?;
        ensure!(
            named.is_file()
                && !named.file_type().is_symlink()
                && (named.dev(), named.ino()) == (held.dev(), held.ino()),
            "unsafe_index: replacement inode changed"
        );
        leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
        self.identity.verify()?;
        let db = open_index(path, false)?;
        let status = self.decode_control_status_raw(&db)?;
        ensure!(
            status.revision == pin && status.evidence_format.is_some(),
            "incompatible_index: staged pair or evidence format changed"
        );
        validate_bounded_control(&db, &self.identity.record_id, PairedManifestScope::Full)?;
        validate_paired_rows(&db)?;
        let integrity: String = db.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
        ensure!(
            integrity == "ok",
            "incompatible_index: staged integrity check failed"
        );
        drop(db);
        leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
        self.identity.verify()?;
        let named = std::fs::symlink_metadata(path)?;
        ensure!(
            named.is_file()
                && !named.file_type().is_symlink()
                && (named.dev(), named.ino()) == (held.dev(), held.ino()),
            "unsafe_index: replacement inode changed"
        );
        Ok(())
    }
    /// Only an already-classified corrupt index under a fresh exclusive leader may enter.
    pub(crate) fn recreate_index_exclusive(
        &self,
        options: &crate::indexer::IndexOptions,
        leader: &mut topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        self.recreate_index_exclusive_with_hook(options, leader, cancel, |_| Ok(()))
    }
    fn recreate_index_exclusive_with_hook(
        &self,
        options: &crate::indexer::IndexOptions,
        leader: &mut topology::WriterGuard,
        cancel: &CancelFlag,
        mut at: impl FnMut(ActivationStage) -> Result<()>,
    ) -> Result<IndexPin> {
        ensure!(
            matches!(
                self.disposition(),
                RecoveryDisposition::RecreatePending | RecoveryDisposition::RootReplaced
            ) && self.recovery_required.load(Ordering::Acquire),
            "recovery_required: exceptional recreation not classified"
        );
        leader.belongs_to(&self.identity)?;
        leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
        self.identity.verify()?;
        ensure!(
            std::fs::canonicalize(&options.workspace_root)? == self.identity.root,
            "root_changed: index options select another workspace"
        );
        check_cancel(cancel)?;
        let path = self.roots.index_db(&self.identity);
        let old = IndexFileWitness::open(&path)?;
        for suffix in ["-wal", "-shm"] {
            let sidecar = path.with_file_name(format!("index.db{suffix}"));
            ensure!(
                !index_path_present(&sidecar)?,
                "recovery_required: unsupported index sidecar: {}",
                sidecar.display()
            );
        }
        let journal_path = path.with_file_name("index.db-journal");
        let journal = index_path_present(&journal_path)?
            .then(|| IndexFileWitness::open(&journal_path))
            .transpose()?;
        // Reclassify after EX admission. A later repair, root mismatch, hot journal,
        // or ordinary format/schema mismatch cannot authorize inode replacement.
        // Bind the marker classified at admission to this EX attempt. A different
        // obsolete marker on the same inode cannot borrow the earlier authority.
        let initial_marker = self.obsolete_format_marker.lock().unwrap().clone();
        let mut obsolete = None;
        let cause = match open_index(&path, false) {
            Err(error) => {
                if let Some(found) = error.downcast_ref::<ObsoleteIndexFormat>() {
                    ensure!(
                        initial_marker.as_ref() == Some(found),
                        "recovery_required: obsolete marker changed since admission"
                    );
                    let db = open_index_marker_probe(&path, false, Duration::ZERO)?;
                    self.verify_obsolete_format(&db, found)?;
                    obsolete = Some(found.clone());
                }
                let class = recovery_class(&error);
                if class == RecoveryClass::Hard
                    && self.disposition() != RecoveryDisposition::RootReplaced
                {
                    return Err(error);
                }
                class
            }
            Ok(db) => {
                let result = self.recovery_baseline_full(&db);
                drop(db);
                match result {
                    Err(error) => {
                        let class = recovery_class(&error);
                        if class == RecoveryClass::Hard
                            && self.disposition() != RecoveryDisposition::RootReplaced
                        {
                            return Err(error);
                        }
                        class
                    }
                    Ok(_) if self.disposition() == RecoveryDisposition::RecreatePending => {
                        // Control reads cannot detect corruption inside FTS segments.
                        // Reconfirm it under EX without changing the live index.
                        let checked = (|| -> Result<()> {
                            let db = open_index(&path, true)?;
                            let integrity: String =
                                storage_result(
                                    db.query_row("PRAGMA quick_check", [], |row| row.get(0)),
                                )?;
                            ensure!(integrity == "ok", ExceptionalIndexFormat);
                            symbol_trigrams::verify_integrity(&db)
                        })();
                        match checked {
                            Err(error)
                                if recovery_class(&error) == RecoveryClass::RecreatePending =>
                            {
                                RecoveryClass::RecreatePending
                            }
                            Err(error) => return Err(error),
                            Ok(()) => RecoveryClass::Rebuild,
                        }
                    }
                    Ok(_) => RecoveryClass::Rebuild,
                }
            }
        };
        if self.disposition() == RecoveryDisposition::RootReplaced {
            // A new inode at the same canonical spelling is a root transition, not
            // corruption. Recheck the old derived index under EX before replacing it.
            let db = open_index(&path, false)?;
            match self.recovery_baseline_full(&db) {
                Err(error) if error.to_string() == "root_changed: index root identity mismatch" => {
                }
                _ => anyhow::bail!("root_changed: replacement authority changed"),
            }
            ensure!(
                journal.is_none(),
                "recovery_required: root replacement has hot journal"
            );
        } else if cause == RecoveryClass::Rebuild {
            // The exclusive probe did not confirm file-level damage. Keep the
            // recovery fence, but rebuild in place instead of replacing the inode.
            check_cancel(cancel)?;
            old.verify()?;
            self.identity.verify()?;
            leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
            leader.downgrade_use_to_shared()?;
            self.recovery_disposition
                .compare_exchange(
                    RecoveryDisposition::RecreatePending as u8,
                    RecoveryDisposition::Rebuild as u8,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .map_err(|_| anyhow::anyhow!("recovery_required: disposition changed under EX"))?;
            let _publication = self.enter_publication(cancel, Duration::from_millis(250))?;
            let expected = self.publication_index_baseline()?;
            let capture = crate::capture::Capture::admit(options, cancel, &|_| {})?;
            let spool = crate::native_evidence::spool_full_capture(
                options,
                &capture,
                &self.identity.root,
                self.root_id(),
                cancel,
                &|_| {},
                |_, _| {},
                &self.native_spool_dir()?,
            )?;
            return self
                .publish_validated_stream_recovery(&spool, &capture, leader, expected, cancel);
        } else {
            ensure!(
                cause == RecoveryClass::RecreatePending,
                "recovery_required: corruption/obsolete marker authority not verified"
            );
        }
        old.verify()?;
        leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
        if let Some(marker) = &obsolete {
            old.verify()?;
            let db = open_index_marker_probe(&path, false, Duration::ZERO)?;
            self.verify_obsolete_format(&db, marker)?;
        }
        let mut stage = self.create_staged_index(leader, true)?;
        let result = (|| -> Result<IndexPin> {
            // Rebuild from the same bounded, sorted document spool as a normal
            // full publication. No facts or projections from the old index are
            // usable here; the private stage remains the sole SQL target.
            let capture = crate::capture::Capture::admit(options, cancel, &|_| {})?;
            let spool = crate::native_evidence::spool_full_capture(
                options,
                &capture,
                &self.identity.root,
                self.root_id(),
                cancel,
                &|_| {},
                |_, _| {},
                &self.native_spool_dir()?,
            )?;
            let pin =
                self.publish_validated_stream_to_stage(&spool, &capture, &stage, leader, cancel)?;
            ensure!(
                pin.index_revision == 1,
                "recovery_required: replacement must start at revision one"
            );
            self.validate_replacement_index(&stage.path, &stage, leader, pin)?;
            stage.file.sync_all()?;
            stage.verify_path()?;
            old.verify()?;
            if let Some(journal) = &journal {
                journal.verify()?;
            }
            check_cancel(cancel)?;
            capture.verify(cancel)?;
            self.identity.verify()?;
            leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
            at(ActivationStage::BeforeRename)?;
            old.verify()?;
            stage.verify_path()?;
            // The publisher's earlier validation is not enough: a failed or hot
            // staged journal appearing at this barrier must never reach index.db.
            reject_sidecars(&stage.path, true)?;
            check_cancel(cancel)?;
            capture.verify(cancel)?;
            self.identity.verify()?;
            leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
            let dir = self.roots.index_dir(&self.identity);
            let backup = if let Some(journal) = &journal {
                let backup = dir.join(format!("index.db-journal.tmp-{}", uuid::Uuid::new_v4()));
                ensure!(
                    !index_path_present(&backup)?,
                    "unsafe_index: journal backup exists"
                );
                journal.verify()?;
                std::fs::rename(&journal.path, &backup)?;
                if let Err(error) = journal.verify_at(&backup) {
                    restore_index_journal(journal, &backup, &dir)
                        .context("incomplete_recovery: journal restoration failed")?;
                    return Err(error);
                }
                // Make the retained journal backup durable before replacing its old
                // index. On any failure before the live rename, restore its pathname.
                let durable_backup = at(ActivationStage::AfterJournalBackupBeforeDirFsync)
                    .and_then(|_| reject_sidecars(&stage.path, true))
                    .and_then(|_| {
                        std::fs::File::open(&dir)?.sync_all()?;
                        Ok(())
                    });
                if let Err(error) = durable_backup {
                    restore_index_journal(journal, &backup, &dir)
                        .context("incomplete_recovery: journal restoration failed")?;
                    return Err(error);
                }
                Some(backup)
            } else {
                None
            };
            if let Some(marker) = &obsolete {
                let recheck = (|| -> Result<()> {
                    old.verify()?;
                    let db = open_index_marker_probe(&path, false, Duration::ZERO)?;
                    self.verify_obsolete_format(&db, marker)?;
                    stage.verify_path()?;
                    reject_sidecars(&stage.path, true)?;
                    reject_sidecars(&path, true)?;
                    leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
                    check_cancel(cancel)
                })();
                if let Err(error) = recheck {
                    if let (Some(journal), Some(backup)) = (&journal, &backup) {
                        restore_index_journal(journal, backup, &dir)
                            .context("incomplete_recovery: journal restoration failed")?;
                    }
                    return Err(error);
                }
            }
            if let Err(error) = replace_index_and_release_obsolete(
                old,
                &stage.path,
                leader,
                &self.roots,
                &self.identity,
            ) {
                if let (Some(journal), Some(backup)) = (&journal, &backup) {
                    restore_index_journal(journal, backup, &dir)
                        .context("incomplete_recovery: journal restoration failed")?;
                }
                return Err(error);
            }
            // The atomic rename may already be durable even if any later step fails.
            // Never let the stage guard unlink the now-live replacement inode.
            stage.published = true;
            at(ActivationStage::AfterRenameBeforeDirFsync)?;
            std::fs::File::open(&dir)?.sync_all()?;
            if let (Some(journal), Some(backup)) = (&journal, &backup) {
                journal.verify_at(backup)?;
                std::fs::remove_file(backup)?;
                std::fs::File::open(&dir)?.sync_all()?;
            }
            self.published_admission.invalidate();
            self.validate_replacement_index(&path, &stage, leader, pin)?;
            leader.downgrade_use_to_shared()?;
            leader.belongs_to(&self.identity)?;
            self.identity.verify()?;
            // This order means every clone still refuses while recovery_required is true.
            self.recovery_disposition
                .store(RecoveryDisposition::Ready as u8, Ordering::Release);
            self.recovery_required.store(false, Ordering::Release);
            Ok(pin)
        })();
        if result.is_err() && !stage.published {
            self.cleanup_failed_staged_index(stage, Some(leader))?;
        }
        result
    }
    fn initialize(
        &self,
        leader: &topology::WriterGuard,
        before_publish: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<()> {
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        let path = self.roots.index_db(&self.identity);
        reject_sidecars(&path, true)?;
        if index_path_present(&path)? {
            return Ok(());
        }
        // Stage creation precedes SH admission so its own failure can seek EX
        // to release a deleted inode without upgrading a held shared guard.
        let mut staged = self.create_staged_index(leader, false)?;
        let use_guard = match self.roots.index_use_existing(&self.identity) {
            Ok(guard) => guard,
            Err(error) => {
                *self.aborted_staged_index.lock().unwrap() = Some(staged);
                return Err(error);
            }
        };
        let result = (|| -> Result<()> {
            before_publish(&staged.path)?;
            verify_index_file(&staged.path)?;
            let mut checked = open_index(&staged.path, true)?;
            let checked_snapshot = storage_result(checked.transaction())?;
            self.decode_control_status_raw(&checked_snapshot)?;
            let integrity: String =
                storage_result(checked_snapshot.query_row("PRAGMA quick_check", [], |r| r.get(0)))?;
            ensure!(
                integrity == "ok",
                "incompatible_index: staged integrity check failed"
            );
            drop(checked_snapshot);
            drop(checked);
            staged.file.sync_all()?;
            // The verified pathname must still refer to the inode we created.
            use std::os::unix::fs::MetadataExt;
            let named = std::fs::symlink_metadata(&staged.path)?;
            let opened = staged.file.metadata()?;
            ensure!(
                named.is_file() && named.dev() == opened.dev() && named.ino() == opened.ino(),
                "unsafe_index: staged pathname changed"
            );
            leader.verify()?;
            use_guard.verify()?;
            self.identity.verify()?;
            ensure!(
                !index_path_present(&path)?,
                "incompatible_index: index appeared during initialization"
            );
            {
                let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
                ensure!(
                    registry
                        .live
                        .keys()
                        .all(|(named, _, _)| named != &staged.path),
                    "storage_busy: staged index still has live SQLite connections"
                );
                ensure!(
                    registry.live.keys().all(|(named, _, _)| named != &path),
                    "storage_busy: prior index incarnation still has live SQLite connections"
                );
                let files = registry
                    .by_path
                    .get(&staged.path)
                    .context("unsafe_index: staged witness missing")?;
                ensure!(
                    files.len() == 1,
                    "unsafe_index: unexpected staged witness count"
                );
                std::fs::rename(&staged.path, &path)?;
                let files = registry
                    .by_path
                    .remove(&staged.path)
                    .expect("verified staged witness");
                registry
                    .by_path
                    .entry(path.clone())
                    .or_default()
                    .extend(files);
            }
            staged.published = true;
            std::fs::File::open(self.roots.index_dir(&self.identity))?.sync_all()?;
            Ok(())
        })();
        drop(use_guard);
        if result.is_err() && !staged.published {
            *self.aborted_staged_index.lock().unwrap() = Some(staged);
        }
        result
    }
    pub(crate) fn writer(&self) -> Result<topology::WriterGuard> {
        self.ensure_not_recreate_pending()?;
        self.roots.writer(self.identity.clone())
    }

    /// Direct publication fixture; production callers use the daemon scheduler.
    #[cfg(feature = "test-support")]
    pub fn publication_for_tests(&self) -> Result<topology::WriterGuard> {
        self.writer()
    }

    /// Exercise daemon-local startup reconciliation and FIFO in isolated integration fixtures.
    #[cfg(feature = "test-support")]
    pub fn writer_session_for_tests(&self) -> Result<Arc<topology::WriterSession>> {
        self.writer_session()
    }

    pub(crate) fn writer_session(&self) -> Result<Arc<topology::WriterSession>> {
        let writer = self.writer()?;
        // startup reconciliation must commit and attest a selected head before this local session
        // may claim a durable request. Reads admit the validated prior head.
        self.published_admission.invalidate();
        self.recovery_required.store(true, Ordering::Release);
        Ok(Arc::new(topology::WriterSession::writer(writer)))
    }
    fn cache(&self) -> Result<IndexConnection> {
        self.connect_index(false)
    }
    /// Roll back the hot journal a killed publication left (Decision 0008 §3).
    /// Under the exclusive use lock no connection in any process uses this
    /// index, so the journal belongs to the witnessed `index.db` inode: SQLite
    /// rolls it back on the first read of one writable connection. That open
    /// neither rejects the journal nor requires a valid header first, since
    /// rollback may restore the header. Busy, full-disk and I/O failures return
    /// typed errors and defer recovery; none of them authorizes recreation.
    /// The recovered head is then admitted and validated as usual.
    fn recover_hot_index_journal(&self) -> Result<()> {
        let path = self.roots.index_db(&self.identity);
        let journal = path.with_file_name("index.db-journal");
        let exclusive = self.roots.index_use_exclusive_existing(&self.identity)?;
        if !index_path_present(&journal)? {
            return Ok(());
        }
        self.published_admission.invalidate();
        for suffix in ["-wal", "-shm"] {
            let sidecar = path.with_file_name(format!("index.db{suffix}"));
            ensure!(
                !index_path_present(&sidecar)?,
                "recovery_required: unsupported index sidecar: {}",
                sidecar.display()
            );
        }
        let witness = IndexFileWitness::open(&path)?;
        self.identity.verify()?;
        (|| -> Result<()> {
            let db = protected_sqlite_open(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
                    | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            storage_result(db.busy_timeout(Duration::from_secs(5)))?;
            storage_result(
                db.query_row("SELECT count(*) FROM sqlite_master", [], |row| {
                    row.get::<_, i64>(0)
                }),
            )?;
            Ok(())
        })()
        .map_err(|error| {
            // Contention keeps its direct type so callers retry it.
            if error.downcast_ref::<SqliteContention>().is_some() {
                error
            } else {
                error.context("recovery_deferred: hot index journal rollback failed")
            }
        })?;
        witness.verify()?;
        ensure!(
            !index_path_present(&journal)?,
            "{HOT_INDEX_JOURNAL}: rollback left the journal in place"
        );
        self.published_admission.invalidate();
        drop(exclusive);
        Ok(())
    }
    fn cache_write(&self) -> Result<IndexConnection> {
        self.connect_index(true)
    }
    fn connect_index(&self, writable: bool) -> Result<IndexConnection> {
        (|| {
            self.ensure_not_recreate_pending()?;
            self.identity.verify()?;
            let use_guard = self.roots.index_use_existing(&self.identity)?;
            let path = self.roots.index_db(&self.identity);
            if !writable {
                READ_GATE.with(|gate| *gate.borrow_mut() = Arc::downgrade(&self.publication_gate));
            }
            let opened = if writable {
                open_index(&path, writable)
            } else {
                retry_stale_hot_journal(&path, Duration::from_secs(5), || open_index(&path, false))
            };
            let db = match opened {
                Ok(db) => db,
                Err(error)
                    if self.disposition() == RecoveryDisposition::Rebuild
                        && recovery_class(&error) == RecoveryClass::Rebuild =>
                {
                    open_index_marker_probe(&path, writable, Duration::from_secs(5))?
                }
                Err(error) => return Err(error),
            };
            self.identity.verify()?;
            use_guard.verify()?;
            Ok(IndexConnection {
                db,
                _use_guard: use_guard,
            })
        })()
        .inspect_err(|error| {
            self.note_index_failure(error);
        })
    }
    fn disposition(&self) -> RecoveryDisposition {
        match self.recovery_disposition.load(Ordering::Acquire) {
            0 => RecoveryDisposition::Ready,
            1 => RecoveryDisposition::Rebuild,
            2 => RecoveryDisposition::RecreatePending,
            3 => RecoveryDisposition::RootReplaced,
            _ => unreachable!("invalid recovery disposition"),
        }
    }
    fn mark_recovery(&self, disposition: RecoveryDisposition) {
        self.recovery_disposition
            .fetch_max(disposition as u8, Ordering::AcqRel);
        let wake = !self.recovery_required.swap(true, Ordering::AcqRel);
        self.published_admission.invalidate();
        if wake {
            self.wake_queue_workers();
        }
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn recovery_pending_for_tests(&self) -> bool {
        self.recovery_required.load(Ordering::Acquire)
    }
    fn ensure_not_recreate_pending(&self) -> Result<()> {
        ensure!(
            !matches!(
                self.disposition(),
                RecoveryDisposition::RecreatePending | RecoveryDisposition::RootReplaced
            ),
            "recovery_required: exceptional index recovery deferred"
        );
        Ok(())
    }
    fn classify_admission_error(&self, error: anyhow::Error) -> Result<bool> {
        match self.note_index_failure(&error) {
            RecoveryClass::Rebuild | RecoveryClass::RecreatePending => Ok(false),
            RecoveryClass::Hard => Err(error),
        }
    }
    /// Only errors from derived-index reads enter this boundary. Caller,
    /// durable-record and operational failures retain their original meaning.
    pub(crate) fn report_index_failure(&self, error: anyhow::Error) -> anyhow::Error {
        self.note_index_failure(&error);
        error
    }
    pub(crate) fn is_rebuild_failure(error: &anyhow::Error) -> bool {
        recovery_class(error) == RecoveryClass::Rebuild
    }
    fn note_index_failure(&self, error: &anyhow::Error) -> RecoveryClass {
        let class = recovery_class(error);
        match class {
            RecoveryClass::Rebuild => self.mark_recovery(RecoveryDisposition::Rebuild),
            RecoveryClass::RecreatePending => {
                self.mark_recovery(RecoveryDisposition::RecreatePending)
            }
            RecoveryClass::Hard => {}
        }
        class
    }
    fn verify_obsolete_format(&self, db: &Connection, marker: &ObsoleteIndexFormat) -> Result<()> {
        match marker {
            ObsoleteIndexFormat::EarlierSchema(expected) => {
                let version: u32 =
                    storage_result(db.pragma_query_value(None, "user_version", |row| row.get(0)))?;
                ensure!(
                    version == *expected && (1..DATABASE_SCHEMA_VERSION).contains(&version),
                    "recovery_required: obsolete schema changed since admission"
                );
            }
            ObsoleteIndexFormat::Metadata(expected) => {
                self.verify_metadata_root(db)?;
                ensure!(
                    read_index_format_marker(db)?.eq(expected),
                    "recovery_required: obsolete marker changed since admission"
                );
            }
        }
        Ok(())
    }
    fn verify_metadata_root(&self, db: &Connection) -> Result<()> {
        let (count, typed): (i64, bool) = db.query_row(
            "SELECT count(*),coalesce(min(singleton=1 AND typeof(root_spelling)='text' AND typeof(root_device)='text' AND typeof(root_inode)='text'),0) FROM index_metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        control_ensure!(
            count == 1,
            "incompatible_index: index root identity is not a singleton"
        );
        let device = self.identity.device.to_string();
        let inode = self.identity.inode.to_string();
        for (column, expected, mismatch) in [
            (
                "root_spelling",
                self.workspace_root.as_bytes(),
                "root_key_collision: index belongs to a different spelling",
            ),
            (
                "root_device",
                device.as_bytes(),
                "root_changed: index root identity mismatch",
            ),
            (
                "root_inode",
                inode.as_bytes(),
                "root_changed: index root identity mismatch",
            ),
        ] {
            let text: bool = typed
                || db.query_row(
                    &format!(
                        "SELECT typeof({column})='text' FROM index_metadata WHERE singleton=1"
                    ),
                    [],
                    |row| row.get(0),
                )?;
            if !text {
                continue;
            }
            let blob = db.blob_open(rusqlite::MAIN_DB, "index_metadata", column, 1, true)?;
            ensure!(blob.len() == expected.len(), "{mismatch}");
            for (offset, chunk) in expected.chunks(64 * 1024).enumerate() {
                let offset = offset
                    .checked_mul(64 * 1024)
                    .context("root identity offset overflow")?;
                let mut actual = vec![0; chunk.len()];
                blob.read_at_exact(&mut actual, offset)?;
                ensure!(actual == chunk, "{mismatch}");
            }
            blob.close()?;
        }
        control_ensure!(
            typed,
            "incompatible_index: index root identity is not typed"
        );
        Ok(())
    }

    fn metadata_atom(&self, db: &Connection, column: MetadataColumn) -> Result<MetadataAtom> {
        use rusqlite::types::ValueRef;
        use sha2::{Digest, Sha256};
        let (type_sql, value_sql, column_name, tag) = column.sql();
        let storage: String = db.query_row(type_sql, [], |row| row.get(0))?;
        match storage.as_str() {
            "null" => Ok(MetadataAtom::Null),
            "integer" => db
                .query_row(value_sql, [], |row| match row.get_ref(0)? {
                    ValueRef::Integer(value) => Ok(MetadataAtom::Integer(value)),
                    _ => Err(rusqlite::Error::InvalidQuery),
                })
                .map_err(Into::into),
            "real" => db
                .query_row(value_sql, [], |row| match row.get_ref(0)? {
                    ValueRef::Real(value) => Ok(MetadataAtom::Real(value.to_bits())),
                    _ => Err(rusqlite::Error::InvalidQuery),
                })
                .map_err(Into::into),
            "text" | "blob" => {
                let text = storage == "text";
                let blob =
                    db.blob_open(rusqlite::MAIN_DB, "index_metadata", column_name, 1, true)?;
                let byte_length = blob.len();
                let mut digest = Sha256::new();
                digest.update(b"trellis-index-metadata-witness-v1\0");
                digest.update(tag);
                digest.update([u8::from(text)]);
                digest.update((byte_length as u64).to_le_bytes());
                let mut offset = 0usize;
                let mut buffer = vec![0; (64 * 1024).min(byte_length)];
                while offset < byte_length {
                    let count = buffer.len().min(byte_length - offset);
                    blob.read_at_exact(&mut buffer[..count], offset)?;
                    digest.update(&buffer[..count]);
                    offset = offset
                        .checked_add(count)
                        .context("metadata offset overflow")?;
                }
                blob.close()?;
                Ok(MetadataAtom::Streamed {
                    column,
                    text,
                    byte_length,
                    sha256: digest.finalize().into(),
                })
            }
            _ => anyhow::bail!("incompatible_index: unknown SQLite metadata storage class"),
        }
    }

    fn metadata_witness(&self, db: &Connection, schema: u32) -> Result<RecoveryWitness> {
        Ok(RecoveryWitness {
            pragma_schema: schema,
            schema_version: self.metadata_atom(db, MetadataColumn::SchemaVersion)?,
            extractor_version: self.metadata_atom(db, MetadataColumn::ExtractorVersion)?,
            index_generation: self.metadata_atom(db, MetadataColumn::IndexGeneration)?,
            index_revision: self.metadata_atom(db, MetadataColumn::IndexRevision)?,
        })
    }

    fn recovery_baseline(&self, db: &Connection) -> Result<RecoveryBaseline> {
        self.recovery_baseline_scoped(db, PairedManifestScope::Changing)
    }

    fn recovery_baseline_preflight(&self, db: &Connection) -> Result<RecoveryBaseline> {
        self.recovery_baseline_scoped(db, PairedManifestScope::Preflight)
    }

    fn recovery_baseline_full(&self, db: &Connection) -> Result<RecoveryBaseline> {
        self.recovery_baseline_scoped(db, PairedManifestScope::Full)
    }

    fn recovery_baseline_scoped(
        &self,
        db: &Connection,
        scope: PairedManifestScope,
    ) -> Result<RecoveryBaseline> {
        let schema: u32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        let metadata_missing: bool = db.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM index_metadata)",
            [],
            |row| row.get(0),
        )?;
        if metadata_missing && schema == DATABASE_SCHEMA_VERSION {
            self.mark_recovery(RecoveryDisposition::Rebuild);
            return Ok(RecoveryBaseline {
                witness: RecoveryWitness {
                    pragma_schema: schema,
                    schema_version: MetadataAtom::Null,
                    extractor_version: MetadataAtom::Null,
                    index_generation: MetadataAtom::Null,
                    index_revision: MetadataAtom::Null,
                },
                pin: None,
                compatible: false,
            });
        }
        let root_valid = match self.verify_metadata_root(db) {
            Ok(()) => true,
            Err(error) if error.is::<ControlIntegrity>() => false,
            Err(error) => return Err(error),
        };
        let witness = self.metadata_witness(db, schema)?;
        if schema > DATABASE_SCHEMA_VERSION {
            anyhow::bail!("incompatible_index: unsupported newer schema cannot be rebaselined");
        }
        if !root_valid || schema != DATABASE_SCHEMA_VERSION {
            self.mark_recovery(RecoveryDisposition::Rebuild);
            return Ok(RecoveryBaseline {
                witness,
                pin: None,
                compatible: false,
            });
        }
        if let Err(error) = validate_cache_shape(db) {
            if recovery_class(&error) != RecoveryClass::Rebuild {
                return Err(error);
            }
            self.mark_recovery(RecoveryDisposition::Rebuild);
            return Ok(RecoveryBaseline {
                witness,
                pin: None,
                compatible: false,
            });
        }
        let bounded_text = |atom: &MetadataAtom, max: usize| matches!(atom, MetadataAtom::Streamed { text: true, byte_length, .. } if *byte_length <= max);
        if !bounded_text(&witness.extractor_version, 256)
            || !bounded_text(&witness.index_generation, 64)
            || matches!(
                witness.schema_version,
                MetadataAtom::Streamed { .. } | MetadataAtom::Null
            )
            || matches!(
                witness.index_revision,
                MetadataAtom::Streamed { .. } | MetadataAtom::Null
            )
        {
            self.mark_recovery(RecoveryDisposition::Rebuild);
            return Ok(RecoveryBaseline {
                witness,
                pin: None,
                compatible: false,
            });
        }
        let decoded: rusqlite::Result<(i64, String, String, i64)> = db.query_row(
            "SELECT schema_version,extractor_version,index_generation,index_revision FROM index_metadata WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        );
        let (metadata_schema, extractor, generation, revision) = match decoded {
            Ok(row) => row,
            Err(error) => {
                let compatible = self.classify_admission_error(error.into())?;
                debug_assert!(!compatible);
                return Ok(RecoveryBaseline {
                    witness,
                    pin: None,
                    compatible: false,
                });
            }
        };
        let generation = match uuid::Uuid::parse_str(&generation) {
            Ok(generation) => generation,
            Err(error) => {
                let compatible = self.classify_admission_error(
                    SelectedIntegrity(format!("incompatible_index: invalid generation: {error}"))
                        .into(),
                )?;
                debug_assert!(!compatible);
                return Ok(RecoveryBaseline {
                    witness,
                    pin: None,
                    compatible: false,
                });
            }
        };
        if !(0..=9_007_199_254_740_991).contains(&revision) {
            self.mark_recovery(RecoveryDisposition::Rebuild);
            return Ok(RecoveryBaseline {
                witness,
                pin: None,
                compatible: false,
            });
        }
        let pin = IndexPin {
            index_generation: generation,
            index_revision: revision as u64,
        };
        let marker_matches = metadata_schema == i64::from(schema);
        let compatible = if marker_matches
            && schema == DATABASE_SCHEMA_VERSION
            && extractor == EXTRACTOR_VERSION
        {
            // Revision zero is the empty v8 bootstrap. It has no native pair
            // yet; the first successful publication creates that metadata.
            let validation = if pin.index_revision == 0 {
                self.decode_control_status_raw(db).and_then(|status| {
                    control_ensure!(
                        status.evidence_format.is_none(),
                        "incompatible_index: bootstrap exposed evidence"
                    );
                    Ok(())
                })
            } else {
                validate_bounded_control(db, &self.identity.record_id, scope)
            };
            match validation {
                Ok(()) => true,
                Err(error) => self.classify_admission_error(error)?,
            }
        } else {
            self.mark_recovery(RecoveryDisposition::Rebuild);
            false
        };
        Ok(RecoveryBaseline {
            witness,
            pin: Some(pin),
            compatible,
        })
    }

    fn decode_control_status_raw(&self, db: &Connection) -> Result<IndexStatus> {
        // This check must run INSIDE the caller's read snapshot or writer lock.
        // The open_index admission check alone cannot protect against later DDL.
        validate_cache_shape(db)?;
        let schema_version: i64 =
            storage_result(db.pragma_query_value(None, "user_version", |r| r.get(0)))?;
        if schema_version == i64::from(DATABASE_SCHEMA_VERSION) {
            bounded_graph_pair(
                db,
                "SELECT typeof(stats),length(CAST(stats AS BLOB)),
                typeof(diagnostics),length(CAST(diagnostics AS BLOB))
                FROM index_metadata WHERE singleton=1",
            )?;
        }
        let row: (i64,String,String,String,String,String,i64,String,String,String) = storage_result(db.query_row(
            "SELECT schema_version,extractor_version,root_spelling,root_device,root_inode,index_generation,index_revision,indexed_at,stats,diagnostics FROM index_metadata WHERE singleton=1",
            [],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?,r.get(6)?,r.get(7)?,r.get(8)?,r.get(9)?))))?;
        control_ensure!(
            schema_version == i64::from(DATABASE_SCHEMA_VERSION)
                && row.0 == schema_version
                && row.1 == EXTRACTOR_VERSION,
            "incompatible_index: extractor or schema"
        );
        ensure!(
            row.2 == self.workspace_root,
            "root_key_collision: index belongs to a different spelling"
        );
        ensure!(
            row.3 == self.identity.device.to_string() && row.4 == self.identity.inode.to_string(),
            "root_changed: index root identity mismatch"
        );
        let pin: IndexPin = serde_json::from_value(
            serde_json::json!({"indexGeneration":row.5,"indexRevision":row.6}),
        )
        .context(SelectedIntegrity(
            "incompatible_index: stored pin decode failed".into(),
        ))?;
        if schema_version == i64::from(DATABASE_SCHEMA_VERSION) {
            if pin.index_revision == 0 {
                validate_v8_bootstrap(db)?;
            } else {
                validate_paired_metadata(
                    db,
                    &self.identity.record_id,
                    PairedManifestScope::Changing,
                )?;
            }
        }
        Ok(IndexStatus {
            workspace_root: self.workspace_root.clone(),
            catching_up: false,
            revision: pin,
            indexed_at: if row.7.is_empty() { None } else { Some(row.7) },
            stats: serde_json::from_str(&row.8).context(SelectedIntegrity(
                "incompatible_index: stored JSON decode failed".into(),
            ))?,
            diagnostics: serde_json::from_str(&row.9).context(SelectedIntegrity(
                "incompatible_index: stored JSON decode failed".into(),
            ))?,
            evidence_format: (row.0 == i64::from(DATABASE_SCHEMA_VERSION) && row.6 > 0)
                .then(|| EVIDENCE_FORMAT.to_owned()),
            watcher: None,
        })
    }
    fn ensure_public_read_ready(&self) -> Result<()> {
        if !self.recovery_required.load(Ordering::Acquire) {
            return Ok(());
        }
        match self.disposition() {
            RecoveryDisposition::Ready => {
                index_diagnostic_stage("public_read_latched");
                Err(topology::IndexNotReady::new("reconciliation required").into())
            }
            RecoveryDisposition::Rebuild => {
                anyhow::bail!(
                    "incompatible_index: reconciliation required after invalid current index"
                )
            }
            RecoveryDisposition::RecreatePending => {
                anyhow::bail!("recovery_required: exceptional index recovery deferred")
            }
            RecoveryDisposition::RootReplaced => {
                anyhow::bail!("recovery_required: replacement-root index recovery deferred")
            }
        }
    }
    fn read_public_control_status(&self, db: &Connection) -> Result<IndexStatus> {
        self.verify_metadata_root(db)
            .map_err(|error| self.report_index_failure(error))?;
        self.ensure_public_read_ready()?;
        self.decode_control_status_raw(db)
            .map_err(|error| self.report_index_failure(error))
    }

    fn read_status(&self, db: &Connection) -> Result<IndexStatus> {
        self.read_status_for(db, false)
    }
    fn read_status_for(&self, db: &Connection, pre_startup: bool) -> Result<IndexStatus> {
        self.cached_read_status_for(db, pre_startup)
            .map_err(|error| self.report_index_failure(error))
    }
    fn read_status_uncached(&self, db: &Connection, pre_startup: bool) -> Result<IndexStatus> {
        #[cfg(test)]
        STATUS_VALIDATIONS.with(|count| count.set(count.get() + 1));
        let status = if pre_startup {
            self.verify_metadata_root(db)
                .map_err(|error| self.report_index_failure(error))?;
            // startup reconciliation may keep a valid prior head readable, but a selected document
            // already proved corrupt cannot be admitted by that same path.
            if self.disposition() == RecoveryDisposition::Rebuild {
                anyhow::bail!(
                    "incompatible_index: reconciliation required after invalid current index"
                );
            }
            self.decode_control_status_raw(db)
                .map_err(|error| self.report_index_failure(error))?
        } else {
            self.read_public_control_status(db)?
        };
        if status.evidence_format.is_none() {
            let schema: i64 =
                storage_result(db.pragma_query_value(None, "user_version", |row| row.get(0)))?;
            // read_public_control_status has already validated the exact empty
            // v8 bootstrap in this snapshot. Only that state can make durable
            // saved reads available without an index; legacy/partial evidence
            // must not take this typed fallback.
            if schema == i64::from(DATABASE_SCHEMA_VERSION) && status.revision.index_revision == 0 {
                return Err(topology::IndexNotReady::new("reindex required").into());
            }
            anyhow::bail!("index_not_ready: reindex required");
        }
        (|| -> Result<()> {
            validate_reconcile_inventory(db)?;
            // Every public v8 derived read needs the same bounded catalog
            // singleton. Missing/oversized live metadata is corruption, never an
            // old-index "requireIndex" fallback. This is one indexed metadata row.
            let warnings_bytes: Option<i64> = db
                .query_row(
                    "SELECT length(CAST(r.class_warnings AS BLOB)) FROM native_revisions r JOIN index_metadata m ON m.index_revision=r.published_index_revision AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            if !warnings_bytes.is_some_and(|bytes| (0..=256 * 1024).contains(&bytes)) {
                return Err(SelectedIntegrity(
                    "incompatible_index: class catalog byte budget exceeded or missing".into(),
                )
                .into());
            }
            Ok(())
        })()
        .map_err(|error| self.report_index_failure(error))?;
        Ok(status)
    }

    /// Internal control baseline, never returned by public status or evidence reads.
    pub fn index_baseline(&self) -> Result<IndexPin> {
        self.recovery_index_baseline()?
            .pin
            .context("index_not_ready: prior index pin is not decodable")
    }
    pub(crate) fn recovery_index_baseline(&self) -> Result<RecoveryBaseline> {
        (|| {
            self.ensure_not_recreate_pending()?;
            let mut db = self.cache()?;
            let tx = storage_result(db.transaction())?;
            begin_read_snapshot(
                &tx,
                &self.roots.index_db(&self.identity),
                Duration::from_secs(5),
            )?;
            self.recovery_baseline(&tx)
        })()
        .map_err(|error| self.report_index_failure(error))
    }
    // Only publication callers may defer retained/header checks: the selected
    // head is verified at open and the resulting head is checked before commit.
    pub(crate) fn publication_index_baseline(&self) -> Result<RecoveryBaseline> {
        (|| {
            self.ensure_not_recreate_pending()?;
            let mut db = self.cache()?;
            let tx = storage_result(db.transaction())?;
            begin_read_snapshot(
                &tx,
                &self.roots.index_db(&self.identity),
                Duration::from_secs(5),
            )?;
            self.recovery_baseline_preflight(&tx)
        })()
        .map_err(|error| self.report_index_failure(error))
    }
    #[cfg(feature = "test-support")]
    pub fn set_ambiguous_settlement_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        self.publication_gate
            .test_before_settlement_confirmation
            .set(hook);
        self.publication_gate
            .test_ambiguous_settlement_once
            .store(true, Ordering::Release);
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn run_ambiguous_settlement_hook_for_tests(&self) -> bool {
        if self
            .publication_gate
            .test_ambiguous_settlement_once
            .swap(false, Ordering::AcqRel)
        {
            self.publication_gate
                .test_before_settlement_confirmation
                .run();
            return true;
        }
        false
    }

    #[cfg(feature = "test-support")]
    #[cfg(test)]
    pub fn set_before_request_terminal_commit_hook_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.test_before_request_terminal_commit_hook.set(hook);
    }

    #[cfg(feature = "test-support")]
    pub fn set_after_claim_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        self.test_after_claim_hook.set(hook);
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn run_after_claim_hook_for_tests(&self) {
        self.test_after_claim_hook.run();
    }

    pub fn root_id(&self) -> &str {
        &self.identity.record_id
    }
    pub(crate) fn root_key(&self) -> &str {
        &self.identity.root_key
    }
    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }
    pub fn recorded_index_options(&self) -> Result<Option<crate::indexer::IndexOptions>> {
        (|| {
            let mut db = self.cache()?;
            let tx = storage_result(db.transaction())?;
            begin_read_snapshot(
                &tx,
                &self.roots.index_db(&self.identity),
                Duration::from_secs(5),
            )?;
            let schema: u32 =
                storage_result(tx.pragma_query_value(None, "user_version", |row| row.get(0)))?;
            if schema != DATABASE_SCHEMA_VERSION {
                return Ok(None);
            }
            // One snapshot must bind the shape, revision and options. A fresh v8
            // bootstrap has no options or evidence; a published revision never may.
            validate_cache_shape(&tx)?;
            if self.recovery_baseline_preflight(&tx)?.pin.is_none() {
                return Ok(None);
            }
            let revision: i64 = storage_result(tx.query_row(
                "SELECT index_revision FROM index_metadata WHERE singleton=1",
                [],
                |row| row.get(0),
            ))?;
            if revision == 0 {
                validate_v8_bootstrap(&tx)?;
                return Ok(None);
            }
            let payload: Option<String> = storage_result(tx.query_row(
                "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
                [],
                |row| row.get(0),
            ))?;
            let payload = payload.ok_or_else(|| {
                ControlIntegrity("incompatible_index: missing reconcile options".into())
            })?;
            let options: crate::indexer::ReconcileOptions = serde_json::from_str(&payload)
                .context(SelectedIntegrity(
                    "incompatible_index: stored JSON decode failed".into(),
                ))
                .context("incompatible_index: invalid reconcile options")?;
            selected_ensure!(
                json(&options)? == payload && options.version == 2,
                "incompatible_index: unsupported reconcile options"
            );
            let mut result =
                crate::indexer::IndexOptions::new(Path::new(&self.workspace_root).to_owned());
            result.max_file_bytes = options.max_file_bytes;
            Ok(Some(result))
        })()
        .map_err(|error| self.report_index_failure(error))
    }
    pub fn verify_root(&self) -> Result<()> {
        self.identity.verify()
    }
    /// Called after the checkout's worker streams stop and its last read ends.
    /// Other Store users may still hold a connection: defer that exact witness
    /// until its protected connection closes, never discard their fcntl locks.
    pub(crate) fn retire_checkout_sqlite_witnesses(&self) {
        let mut registry = sqlite_witnesses().lock().unwrap_or_else(|e| e.into_inner());
        for path in [
            self.roots.index_db(&self.identity),
            self.roots.requests_db(&self.identity),
        ] {
            registry.retired.insert(path.clone());
            clean_retired_sqlite_witness(&mut registry, &path);
        }
    }

    pub(crate) fn root_path_replaced(&self) -> Result<bool> {
        self.identity.root_path_replaced()
    }
    pub(crate) fn is_root_replaced(&self) -> bool {
        self.disposition() == RecoveryDisposition::RootReplaced
    }
    pub(crate) fn is_ready_disposition(&self) -> bool {
        self.disposition() == RecoveryDisposition::Ready
    }
    pub(crate) fn is_recreate_pending(&self) -> bool {
        matches!(
            self.disposition(),
            RecoveryDisposition::RecreatePending | RecoveryDisposition::RootReplaced
        ) && self.recovery_required.load(Ordering::Acquire)
    }
    pub(crate) fn recreate_pending_writer_session(
        &self,
        options: &crate::indexer::IndexOptions,
        cancel: &CancelFlag,
    ) -> Result<(IndexPin, Arc<topology::WriterSession>)> {
        ensure!(
            self.is_recreate_pending(),
            "recovery_required: exceptional recreation not classified"
        );
        // The queue's protected open rolls back a hot journal. No replacement
        // publication or claim is admitted until every foreign-root row is
        // confirmed terminal by a separate queue reread.
        self.settle_foreign_root_requests()?;
        let exclusive = self.roots.index_use_exclusive_existing(&self.identity)?;
        let mut writer = self
            .roots
            .writer_under_exclusive(self.identity.clone(), exclusive)?;
        #[cfg(test)]
        self.test_exclusive_recovery_hook.run();
        let pin = self.recreate_index_exclusive(options, &mut writer, cancel)?;
        let session = Arc::new(topology::WriterSession::writer(writer));
        self.verify_writer_session(&session)?;
        ensure!(
            self.status()?.revision == pin,
            "index_not_ready: exceptional recovery pair not admitted"
        );
        self.attest_post_acquisition_reconciliation(&session, pin)?;
        Ok((pin, session))
    }
    pub(crate) fn verify_writer_session(&self, session: &topology::WriterSession) -> Result<()> {
        session.belongs_to(&self.identity)
    }
    /// Mint a claim proof only from a selected post-COMMIT revision carrying
    /// this exact synced leader incarnation. Merely acquiring EX cannot mint it.
    pub(crate) fn attest_post_acquisition_reconciliation(
        &self,
        session: &topology::WriterSession,
        committed_pin: IndexPin,
    ) -> Result<()> {
        self.verify_writer_session(session)?;
        self.attest_post_acquisition_reconciliation_guard(session.writer_guard()?, committed_pin)
    }
    /// The public full-native publication API also performs a complete guarded
    /// capture and COMMIT. Its callers must receive the same claim authority as
    /// coordinator callers, including an already-held daemon owner.
    fn attest_post_acquisition_reconciliation_guard(
        &self,
        writer: &topology::WriterGuard,
        committed_pin: IndexPin,
    ) -> Result<()> {
        writer.belongs_to(&self.identity)?;
        let db = self.cache()?;
        storage_result(db.execute_batch("BEGIN DEFERRED"))?;
        begin_read_snapshot(
            &db,
            &self.roots.index_db(&self.identity),
            Duration::from_secs(5),
        )?;
        let selected = self.read_revision_for(&db, Some(committed_pin), true)?;
        ensure!(
            selected.pin == committed_pin,
            "index_not_ready: mandatory writer reconciliation not committed"
        );
        self.identity.verify()?;
        writer.belongs_to(&self.identity)?;
        *self.reconciled_writer.lock().unwrap() = Some(writer.incarnation);
        self.recovery_required.store(false, Ordering::Release);
        Ok(())
    }
    /// The public Store::claim_request entry point must refuse pre-COMMIT
    /// claims, including the leader's own request and direct API callers.
    pub(crate) fn verify_committed_writer_claim(
        &self,
        session: &topology::WriterSession,
    ) -> Result<()> {
        self.verify_writer_session(session)?;
        if *self.reconciled_writer.lock().unwrap() != Some(session.incarnation()) {
            return Err(
                topology::IndexNotReady::new("mandatory reconciliation not committed").into(),
            );
        }
        self.verify_writer_session(session)
    }
    fn compose_selected_class_catalog(
        graph: &Graph,
        extractions: &BTreeMap<String, crate::model::FileExtraction>,
        limits: crate::classes::Limits,
    ) -> Result<crate::classes::Catalog> {
        // The local graph materializes only the changed document's nodes.
        // Apply the unchanged class cap to the selected FULL revision's
        // authenticated prior+delta symbol count, not graph.nodes.len().
        crate::classes::Catalog::compose(
            &extractions.values().cloned().collect::<Vec<_>>(),
            graph.files.len(),
            graph.stats.symbols,
            limits,
        )
    }

    #[allow(dead_code)] // Internal diagnostics; exercised by the writer-lock regression test.
    pub(crate) fn last_writer_counters(&self) -> Option<WriterCounters> {
        *self.writer_counters.lock().unwrap()
    }

    /// Release CLI evidence for the last successful publication. Counts are
    /// SQLite bound values, not whole-database payloads or file-system bytes.
    pub fn last_writer_diagnostic(&self) -> Option<String> {
        let counters = (*self.writer_counters.lock().unwrap())?;
        Some(format!(
            "index-writer manifest_rows={} manifest_bind_bytes={} native_rows={} native_bind_bytes={} graph_rows={} graph_bind_bytes={} class_rows={} class_bind_bytes={} total_rows={} total_bind_bytes={} reused_occurrence_reads={}",
            counters.manifest.rows,
            counters.manifest.bytes,
            counters.native.rows,
            counters.native.bytes,
            counters.graph.rows,
            counters.graph.bytes,
            counters.class.rows,
            counters.class.bytes,
            counters.total.rows,
            counters.total.bytes,
            counters.reused_occurrence_reads,
        ))
    }
    #[cfg(test)]
    pub(crate) fn set_native_spool_hook(&self, hook: impl FnOnce() + Send + 'static) {
        self.test_native_spool_hook.set(hook);
    }

    /// Existing private scratch directory for an unpublished full-document spool.
    /// Only Store activation provisions this topology.
    pub(crate) fn native_spool_dir(&self) -> Result<std::path::PathBuf> {
        #[cfg(test)]
        self.test_native_spool_hook.run();
        // The coordinator already owns SH or recovery EX. A second SH
        // admission would conflict with its own exclusive recovery lock.
        self.roots.existing_index_dir(&self.identity)
    }

    /// Directory that holds per-operation semantic stages: the existing index directory.
    pub(crate) fn semantic_stage_parent(&self) -> Result<std::path::PathBuf> {
        self.roots.existing_index_dir(&self.identity)
    }

    pub(crate) fn begin_writer_publication(&self, session: &topology::WriterSession) -> Result<()> {
        // A routine capture does not invalidate the committed head. Public
        // readers keep their own selected snapshot while the writer prepares
        // the next revision; only actual recovery closes that read path.
        // Claim authority still requires the separate reconciled-leader proof.
        self.verify_writer_session(session)
    }
    /// Admit one validated committed head on the daemon's selected SQLite
    /// snapshot. The root and selected revision are rechecked at finish.
    pub fn evidence_response_daemon_local(&self) -> Result<EvidenceResponse> {
        (|| {
            if self.is_recreate_pending() {
                return Err(topology::IndexNotReady::new(
                    "exceptional index reconciliation pending",
                )
                .into());
            }
            self.identity.verify()?;
            let db = self
                .cache()
                .map_err(|error| self.report_index_failure(error))?;
            storage_result(db.execute_batch("BEGIN DEFERRED"))?;
            begin_read_snapshot(
                &db,
                &self.roots.index_db(&self.identity),
                Duration::from_secs(5),
            )?;
            let schema: u32 =
                storage_result(db.pragma_query_value(None, "user_version", |row| row.get(0)))?;
            if schema != DATABASE_SCHEMA_VERSION {
                return Err(topology::IndexNotReady::new("reconciliation required").into());
            }
            let selected = self.read_revision_for(&db, None, true)?;
            self.identity.verify()?;
            let mut store = self.clone();
            store.admitted = Some((selected.pin, selected.headers));
            let optional_recovery = OptionalRecovery::new(&store);
            Ok(EvidenceResponse {
                store,
                db,
                fence: ReadFence(selected.pin),
                optional_recovery,
            })
        })()
        .map_err(|error| self.report_index_failure(error))
    }

    pub fn evidence_response(&self) -> Result<EvidenceResponse> {
        self.evidence_response_daemon_local()
    }

    #[cfg(test)]
    fn set_metrics_document_hook(
        &self,
        hook: impl Fn(&str, MetricsDocumentStage) + Send + Sync + 'static,
    ) {
        *self.test_metrics_document_hook.lock().unwrap() = Some(Arc::new(hook));
    }
    #[cfg(test)]
    fn run_metrics_document_hook(&self, path: &str, stage: MetricsDocumentStage) {
        let hook = self.test_metrics_document_hook.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(path, stage);
        }
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn metrics_memo_entries_for_tests(&self) -> usize {
        self.metrics_memo.lock().unwrap().len()
    }

    fn with_evidence<T>(&self, read: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_evidence_hook(read, || Ok(()))
    }
    fn with_evidence_hook<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T>,
        before_finish: impl FnOnce() -> Result<()>,
    ) -> Result<T> {
        self.with_evidence_observed(read, before_finish, |_| {})
    }
    fn with_evidence_observed<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T>,
        before_finish: impl FnOnce() -> Result<()>,
        observe_finish: impl FnOnce(&Result<()>),
    ) -> Result<T> {
        self.with_evidence_response(|response| read(&response.db), before_finish, observe_finish)
    }
    /// `with_evidence`, reading through the response's own Store, which shares
    /// the admitted revision's header values with reads of the same pin.
    fn with_evidence_response<T>(
        &self,
        read: impl FnOnce(&EvidenceResponse) -> Result<T>,
        before_finish: impl FnOnce() -> Result<()>,
        observe_finish: impl FnOnce(&Result<()>),
    ) -> Result<T> {
        let response = self.evidence_response()?;
        let value = read(&response).map_err(|error| self.report_index_failure(error));
        let hook = before_finish();
        let fence = response.finish(());
        observe_finish(&fence);
        match value {
            Err(error) => Err(error),
            Ok(value) => {
                hook?;
                fence?;
                Ok(value)
            }
        }
    }
    pub fn status(&self) -> Result<IndexStatus> {
        self.status_with_open_hook(|_| Ok(()))
    }
    // Private barrier after the admission check, before the read transaction.
    // A second SQLite connection can add DDL here in regression tests.
    fn status_with_open_hook(
        &self,
        before_snapshot: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<IndexStatus> {
        self.ensure_not_recreate_pending()?;
        let db = self
            .cache()
            .map_err(|error| self.report_index_failure(error))?;
        before_snapshot(&db)?;
        drop(db);
        self.with_evidence(|db| self.read_status(db))
    }
    pub fn publish(
        &self,
        graph: &Graph,
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let _ = (graph, leader, expected_revision, cancel);
        anyhow::bail!("native_evidence_required: graph-only publication refused")
    }
    pub fn publish_captured(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let _ = (graph, capture, leader, expected_revision, cancel);
        anyhow::bail!("native_evidence_required: captured graph-only publication refused")
    }
    pub fn publish_native(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let pin = self.publish_native_expected(
            graph,
            capture,
            native,
            leader,
            ExpectedPublication::Pin(expected_revision),
            cancel,
            false,
        )?;
        self.attest_post_acquisition_reconciliation_guard(leader, pin)?;
        Ok(pin)
    }
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn publish_native_recovery_for_tests(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let expected = self.publication_index_baseline()?;
        let pin = self.publish_native_expected(
            graph,
            capture,
            native,
            leader,
            ExpectedPublication::Recovery(Box::new(expected)),
            cancel,
            false,
        )?;
        self.attest_post_acquisition_reconciliation_guard(leader, pin)?;
        Ok(pin)
    }
    /// Publishes one joined batch as the next revision: raw rows, semantic rows, selection and
    /// report in a single transaction against `expected`.
    pub fn publish_semantic_batch(
        &self,
        joined: &crate::semantic_batch::JoinedBatch,
        stage: &mut crate::semantic_batch::staging::Stage,
        expected: IndexPin,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        self.publish_semantic_batch_inner(joined, stage, expected, leader, cancel, |_| Ok(()), None)
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_semantic_batch_with_fault_for_tests(
        &self,
        joined: &crate::semantic_batch::JoinedBatch,
        stage: &mut crate::semantic_batch::staging::Stage,
        expected: IndexPin,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        fault: BatchPublishFault,
    ) -> Result<IndexPin> {
        let loss = match fault {
            BatchPublishFault::CommitLostAfterCommit => Some(CommitLoss::After),
            BatchPublishFault::CommitLostBeforeCommit => Some(CommitLoss::Before),
            BatchPublishFault::CommitBusyBeforeCommit => Some(CommitLoss::BusyBefore),
            _ => None,
        };
        self.publish_semantic_batch_inner(
            joined,
            stage,
            expected,
            leader,
            cancel,
            |db| {
                match fault {
                    BatchPublishFault::PrecommitCancel => {
                        cancel.store(true, std::sync::atomic::Ordering::Relaxed)
                    }
                    BatchPublishFault::SqlFault => {
                        db.execute(
                            "INSERT INTO semantic_import_reports(revision_id) VALUES ('forced-sql-fault')",
                            [],
                        )?;
                    }
                    BatchPublishFault::InsertDiskFull => {
                        db.execute("CREATE TABLE full_probe(payload BLOB)", [])?;
                        let pages: i64 = db.query_row("PRAGMA page_count", [], |row| row.get(0))?;
                        let limit: i64 =
                            db.query_row("PRAGMA max_page_count", [], |row| row.get(0))?;
                        db.query_row(&format!("PRAGMA max_page_count={pages}"), [], |_| Ok(()))?;
                        let inserted =
                            db.execute("INSERT INTO full_probe VALUES (zeroblob(1 << 22))", []);
                        db.query_row(&format!("PRAGMA max_page_count={limit}"), [], |_| Ok(()))?;
                        inserted?;
                    }
                    _ => {}
                }
                Ok(())
            },
            loss,
        )
    }

    /// Publishes like `publish_semantic_batch` and lets `observe` read the writer connection just
    /// before commit.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_semantic_batch_observing_for_tests(
        &self,
        joined: &crate::semantic_batch::JoinedBatch,
        stage: &mut crate::semantic_batch::staging::Stage,
        expected: IndexPin,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        observe: impl FnOnce(&Connection) -> Result<()>,
    ) -> Result<IndexPin> {
        let mut observe = Some(observe);
        self.publish_semantic_batch_inner(
            joined,
            stage,
            expected,
            leader,
            cancel,
            |db| observe.take().map_or(Ok(()), |observe| observe(db)),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_semantic_batch_inner(
        &self,
        joined: &crate::semantic_batch::JoinedBatch,
        stage: &mut crate::semantic_batch::staging::Stage,
        expected: IndexPin,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        before_commit: impl FnMut(&Connection) -> Result<()>,
        commit_loss: Option<CommitLoss>,
    ) -> Result<IndexPin> {
        self.publish_semantic_batch_statements(
            joined,
            stage,
            expected,
            leader,
            cancel,
            before_commit,
            commit_loss,
        )
        .map_err(typed_storage_full)
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_semantic_batch_statements(
        &self,
        joined: &crate::semantic_batch::JoinedBatch,
        stage: &mut crate::semantic_batch::staging::Stage,
        expected: IndexPin,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        mut before_commit: impl FnMut(&Connection) -> Result<()>,
        commit_loss: Option<CommitLoss>,
    ) -> Result<IndexPin> {
        use crate::semantic_batch::{descriptor::Descriptor, limits};
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        check_cancel(cancel)?;
        ensure!(
            joined.destination_pin() == expected,
            "revision conflict: semantic batch destination changed"
        );
        let report_json = serde_json::to_string(joined.report())?;
        ensure!(
            report_json.len() <= 1024 * 1024,
            "semantic import report exceeds response bound"
        );
        let batch_limits = batch_publication_limits();
        // No handle on stage.db may exist while the writer has it attached.
        stage.release_connection();
        let stage_uri = stage_attach_uri(&stage.db_path());
        let conn = self.cache_write()?;
        let db: &Connection = &conn;
        storage_result(db.pragma_update(
            None,
            "cache_size",
            -i64::try_from(limits::BATCH_WRITER_CACHE_KIB)?,
        ))?;
        storage_result(db.execute("ATTACH DATABASE ?1 AS stage", [&stage_uri]))?;
        let attachment = StageAttachment(db);
        storage_result(db.pragma_update(
            Some("stage"),
            "cache_size",
            -i64::try_from(limits::STAGE_ATTACH_CACHE_KIB)?,
        ))?;
        let gate = || -> Result<()> {
            check_cancel(cancel)?;
            stage.budget(&batch_limits).check()?;
            Ok(())
        };
        let tx = storage_result(Transaction::new_unchecked(
            db,
            TransactionBehavior::Immediate,
        ))?;
        let current = self.recovery_baseline_preflight(&tx)?;
        ensure!(
            current.pin == Some(expected),
            "revision conflict: semantic batch destination changed"
        );
        let (header_sha, descriptor_bytes): (String, Vec<u8>) = tx.query_row(
            "SELECT descriptor_sha256,descriptor_bytes FROM stage.header",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let attempt: i64 =
            tx.query_row("SELECT attempt FROM stage.join_state", [], |row| row.get(0))?;
        ensure!(
            attempt == joined.attempt() && header_sha == joined.descriptor_sha256(),
            "joined batch superseded by a later join attempt"
        );
        let descriptor = Descriptor::parse(&descriptor_bytes)?;
        let producer = descriptor.check_producer()?.producer();
        let old_key = semantic_pin_key(expected);
        let head_native: String = tx.query_row(
            "SELECT native_revision_id FROM native_revisions WHERE id=?1",
            [&old_key],
            |row| row.get(0),
        )?;
        ensure!(
            head_native == joined.destination_revision_id(),
            "revision conflict: semantic batch destination changed"
        );
        let captured_key = semantic_pin_key(joined.captured_pin());
        let retained: Option<String> = tx
            .query_row(
                "SELECT native_revision_id FROM native_revisions r WHERE id=?1 AND source_set_id=?2 AND NOT EXISTS(SELECT 1 FROM revision_capture_inputs WHERE revision_id=r.id AND input_key IN ('__released:v1','__pending_release:v1'))",
                params![captured_key, descriptor.source_set.id],
                |row| row.get(0),
            )
            .optional()?;
        ensure!(
            retained.as_deref() == Some(descriptor.source_revision.as_str()),
            "capture_unavailable: captured revision not retained"
        );
        gate()?;
        let next = IndexPin {
            index_generation: expected.index_generation,
            index_revision: expected
                .index_revision
                .checked_add(1)
                .filter(|n| *n <= 9_007_199_254_740_991)
                .context("revision overflow")?,
        };
        let next_key = semantic_pin_key(next);
        tx.execute("INSERT INTO native_revisions SELECT ?1,source_set_id,toolchain_hash,config_hash,dependency_hash,native_revision_id,source_inventory,dependency_observations,retained_toolchain,retained_config,retained_dependencies,reconcile_options,class_warnings,class_truncated,graph_stats,graph_diagnostics,?2 FROM native_revisions WHERE id=?3",params![next_key,next.index_revision as i64,old_key])?;
        tx.execute("INSERT INTO revision_documents SELECT ?1,language,path,vid,gid,cid,capture_stat,coverage_requested,coverage_selected,coverage_state,coverage_diagnostic,ordinal,class_complete FROM revision_documents WHERE revision_id=?2",params![next_key,old_key])?;
        tx.execute("INSERT INTO revision_capture_inputs SELECT ?1,input_key,payload FROM revision_capture_inputs WHERE revision_id=?2",params![next_key,old_key])?;
        let (producer_sha, binding_sha) = revision_producer_binding(&tx, &next_key)?;
        tx.execute(
            "INSERT INTO revision_producer_bindings VALUES(?1,?2,?3)",
            params![next_key, producer_sha, binding_sha],
        )?;
        gate()?;
        tx.execute("INSERT INTO semantic_revision_producers SELECT ?1,id,version FROM semantic_revision_producers WHERE revision_id=?2 AND id<>?3",params![next_key,old_key,producer.id])?;
        tx.execute("INSERT INTO semantic_requested_producers SELECT ?1,id,version,executable_hash FROM semantic_requested_producers WHERE revision_id=?2 AND id<>?3",params![next_key,old_key,producer.id])?;
        tx.execute("INSERT INTO semantic_requested_producer_languages SELECT ?1,id,language,ordinal FROM semantic_requested_producer_languages WHERE revision_id=?2 AND id<>?3",params![next_key,old_key,producer.id])?;
        tx.execute("INSERT INTO semantic_revision_raw_selections SELECT ?1,producer_id,batch_id,captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?2 AND producer_id<>?3",params![next_key,old_key,producer.id])?;
        copy_semantic_rows_except(&tx, &old_key, &next_key, &producer.id)?;
        write_producer_descriptor(&tx, &next_key, &producer)?;
        tx.execute(
            "INSERT INTO semantic_requested_producers VALUES(?1,?2,?3,?4)",
            params![
                next_key,
                producer.id,
                producer.version,
                producer.executable_hash
            ],
        )?;
        for (ordinal, language) in producer.languages.iter().enumerate() {
            tx.execute(
                "INSERT INTO semantic_requested_producer_languages VALUES(?1,?2,?3,?4)",
                params![
                    next_key,
                    producer.id,
                    semantic_variant(language)?,
                    ordinal as i64
                ],
            )?;
        }
        gate()?;
        let batch_id = write_staged_raw_rows(
            &tx,
            stage,
            &descriptor,
            &descriptor_bytes,
            &header_sha,
            &gate,
        )?;
        gate()?;
        write_staged_semantic(&tx, &next_key, &producer, &gate)?;
        tx.execute(
            "INSERT INTO semantic_revision_raw_selections VALUES(?1,?2,?3,?4)",
            params![next_key, producer.id, batch_id, captured_key],
        )?;
        tx.execute("INSERT INTO semantic_import_reports(revision_id,producer_id,report_json) VALUES(?1,?2,?3)",params![next_key,producer.id,report_json])?;
        tx.execute("INSERT INTO semantic_import_reports SELECT ?1,producer_id,report_json FROM semantic_import_reports WHERE revision_id=?2 AND producer_id<>?3",params![next_key,old_key,producer.id])?;
        search::publish_head(&tx, &next_key)?;
        let published_at = self.retention_time()?;
        stamp_predecessor(&tx, &old_key, published_at.as_secs() as i64)?;
        tx.execute("UPDATE index_metadata SET index_revision=?1,indexed_at=?2 WHERE singleton=1 AND index_generation=?3 AND index_revision=?4", params![next.index_revision as i64,published_at.as_millis().to_string(),expected.index_generation.to_string(),expected.index_revision as i64])?;
        gate()?;
        before_commit(&tx)?;
        leader.verify()?;
        self.identity.verify()?;
        check_cancel(cancel)?;
        #[cfg(all(feature = "test-support", unix))]
        if let Some(path) = std::env::var_os("TRELLIS_TEST_SEMANTIC_PAUSE_BEFORE_COMMIT_SOCKET") {
            use std::os::unix::net::UnixListener;
            let path = std::path::PathBuf::from(path);
            if !path.exists() {
                let listener = UnixListener::bind(&path)?;
                std::fs::write(path.with_extension("ready"), [])?;
                let (_stream, _) = listener.accept()?;
            }
        }
        #[allow(unused_mut)]
        let mut loss = commit_loss;
        #[cfg(feature = "test-support")]
        if loss.is_none()
            && std::env::var_os("TRELLIS_TEST_SEMANTIC_AMBIGUOUS_COMMIT").is_some_and(|marker| {
                let marker = std::path::PathBuf::from(marker);
                !marker.exists() && std::fs::write(marker, b"semantic commit succeeded").is_ok()
            })
        {
            loss = Some(CommitLoss::After);
        }
        #[cfg(feature = "test-support")]
        if loss.is_none()
            && std::env::var_os("TRELLIS_TEST_SEMANTIC_COMMIT_BUSY").is_some_and(|marker| {
                let marker = std::path::PathBuf::from(marker);
                !marker.exists() && std::fs::write(marker, b"semantic commit busy").is_ok()
            })
        {
            loss = Some(CommitLoss::BusyBefore);
        }
        #[cfg(feature = "test-support")]
        if loss.is_none()
            && std::env::var_os("TRELLIS_TEST_SEMANTIC_COMMIT_DISK_FULL").is_some_and(|marker| {
                let marker = std::path::PathBuf::from(marker);
                !marker.exists() && std::fs::write(marker, b"semantic commit disk full").is_ok()
            })
        {
            loss = Some(CommitLoss::DiskFullBefore);
        }
        let committed = match loss {
            Some(CommitLoss::DiskFullBefore) => {
                drop(tx);
                Err(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                    Some("database or disk is full".into()),
                )
                .into())
            }
            Some(CommitLoss::Before) => {
                drop(tx);
                Err(anyhow::anyhow!("semantic batch commit result lost"))
            }
            Some(CommitLoss::BusyBefore) => {
                drop(tx);
                Err(topology::StorageBusy.into())
            }
            _ => {
                let outcome = storage_result(tx.commit());
                if outcome.is_ok() && loss == Some(CommitLoss::After) {
                    Err(anyhow::anyhow!("semantic batch commit result lost"))
                } else {
                    outcome
                }
            }
        };
        match committed {
            Ok(()) => Ok(next),
            Err(commit_error) => {
                drop(attachment);
                drop(conn);
                let state = self.batch_commit_state(expected, next, joined.descriptor_sha256())?;
                match state {
                    BatchCommitState::Committed => Ok(next),
                    // A rolled-back COMMIT that failed from contention stays typed
                    // `storage_busy`, so the request is requeued rather than re-joined, which
                    // would charge the shared lookup budget again.
                    BatchCommitState::RolledBack if transient_storage_contention(&commit_error) => {
                        Err(commit_error)
                    }
                    // A full disk or I/O error rolled the COMMIT back. Re-joining cannot help,
                    // so the request fails with the real cause and the prior head stays.
                    BatchCommitState::RolledBack if storage_fault(&commit_error) => {
                        Err(commit_error.context("semantic batch commit rolled back"))
                    }
                    BatchCommitState::RolledBack => {
                        anyhow::bail!("revision conflict: semantic batch commit did not persist")
                    }
                    BatchCommitState::Moved => {
                        anyhow::bail!("revision conflict: semantic batch destination changed")
                    }
                }
            }
        }
    }

    /// Settles a COMMIT whose outcome is unknown by re-reading durable state. It reports success
    /// only when the next revision is the head and selects this descriptor, and a conflict only
    /// when the head is still `expected` or has moved on.
    pub fn resolve_ambiguous_batch_commit(
        &self,
        expected: IndexPin,
        new_pin: IndexPin,
        descriptor_sha256: &str,
    ) -> Result<IndexPin> {
        match self.batch_commit_state(expected, new_pin, descriptor_sha256)? {
            BatchCommitState::Committed => Ok(new_pin),
            BatchCommitState::RolledBack => {
                anyhow::bail!("revision conflict: semantic batch commit did not persist")
            }
            BatchCommitState::Moved => {
                anyhow::bail!("revision conflict: semantic batch destination changed")
            }
        }
    }

    fn batch_commit_state(
        &self,
        expected: IndexPin,
        new_pin: IndexPin,
        descriptor_sha256: &str,
    ) -> Result<BatchCommitState> {
        let head = self.index_baseline()?;
        if head == new_pin {
            let selected = self.with_evidence(|db| {
                Ok(db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s JOIN semantic_raw_batches b ON b.batch_id=s.batch_id AND b.producer_id=s.producer_id WHERE s.revision_id=?1 AND b.descriptor_sha256=?2)",
                    params![semantic_pin_key(head), descriptor_sha256],
                    |row| row.get::<_, bool>(0),
                )?)
            })?;
            if selected {
                return Ok(BatchCommitState::Committed);
            }
        }
        Ok(if head == expected {
            BatchCommitState::RolledBack
        } else {
            BatchCommitState::Moved
        })
    }

    /// The report committed with the batch whose descriptor digest is `descriptor_sha256`, if
    /// that batch is selected at `pin`.
    pub fn semantic_batch_report_at(
        &self,
        pin: IndexPin,
        descriptor_sha256: &str,
    ) -> Result<Option<serde_json::Value>> {
        self.native_at(pin, |db, selected| {
            let report: Option<String> = db
                .query_row(
                    "SELECT r.report_json FROM semantic_import_reports r JOIN semantic_revision_raw_selections s ON s.revision_id=r.revision_id AND s.producer_id=r.producer_id JOIN semantic_raw_batches b ON b.batch_id=s.batch_id AND b.producer_id=s.producer_id WHERE r.revision_id=?1 AND b.descriptor_sha256=?2",
                    params![selected.key, descriptor_sha256],
                    |row| row.get(0),
                )
                .optional()?;
            report
                .map(|payload| {
                    serde_json::from_str(&payload).context(SelectedIntegrity(
                        "incompatible_index: corrupt semantic import report".into(),
                    ))
                })
                .transpose()
        })
    }

    /// Publish captured semantic evidence with a native revision in the same transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn publish_native_semantic(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        semantic: &crate::semantic_evidence::SemanticPublication,
        captured: &crate::semantic_evidence::CapturedComponents,
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        self.publish_native_semantic_with_tx_hook(
            graph,
            capture,
            native,
            semantic,
            captured,
            leader,
            expected_revision,
            cancel,
            |_| Ok(()),
        )
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_native_semantic_with_sql_fault_for_tests(
        &self,
        evidence: (
            &Graph,
            &crate::capture::Capture,
            &crate::native_evidence::Artifact,
            &crate::semantic_evidence::SemanticPublication,
            &crate::semantic_evidence::CapturedComponents,
        ),
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let (graph, capture, native, semantic, captured) = evidence;
        self.publish_native_semantic_with_tx_hook(
            graph,
            capture,
            native,
            semantic,
            captured,
            leader,
            expected_revision,
            cancel,
            |db| {
                db.execute(
                    "INSERT INTO semantic_coverage(revision_id) VALUES ('forced-sql-fault')",
                    [],
                )?;
                Ok(())
            },
        )
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_native_semantic_with_precommit_cancel_for_tests(
        &self,
        evidence: (
            &Graph,
            &crate::capture::Capture,
            &crate::native_evidence::Artifact,
            &crate::semantic_evidence::SemanticPublication,
            &crate::semantic_evidence::CapturedComponents,
        ),
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let (graph, capture, native, semantic, captured) = evidence;
        self.publish_native_semantic_with_tx_hook(
            graph,
            capture,
            native,
            semantic,
            captured,
            leader,
            expected_revision,
            cancel,
            |_| {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_native_semantic_with_tx_hook(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        semantic: &crate::semantic_evidence::SemanticPublication,
        captured: &crate::semantic_evidence::CapturedComponents,
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
        mut hook: impl FnMut(&Connection) -> Result<()>,
    ) -> Result<IndexPin> {
        self.validate_native_bundle(graph, capture, native, cancel)?;
        let pin = self.publish_inner_checked_target_with_local(
            (graph, capture, native),
            leader,
            PublicationPlan {
                expected: ExpectedPublication::Pin(expected_revision),
                target: PublicationTarget::Live,
            },
            cancel,
            (256 * 1024 * 1024 + 16 * 1024, None, None),
            |stage, db| {
                if matches!(stage, PublishStage::BeforeCommit) {
                    hook(db)
                } else {
                    Ok(())
                }
            },
            Some((semantic, captured)),
        )?;
        self.attest_post_acquisition_reconciliation_guard(leader, pin)?;
        Ok(pin)
    }

    /// Publish a validated full revision from document-local evidence. The spool is
    /// private, repeatable and never part of the visible index; SQL remains one
    /// all-or-nothing revision transaction.
    pub(crate) fn publish_validated_stream_recovery(
        &self,
        spool: &crate::native_evidence::FullDocumentSpool,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        expected: RecoveryBaseline,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        self.publish_validated_stream_target(
            spool,
            capture,
            leader,
            expected,
            PublicationTarget::Live,
            cancel,
            None,
            |_, _| Ok(()),
        )
    }

    /// A replacement starts from an empty bootstrap under exclusive use. In
    /// particular, it never consults corrupt live rows for reuse or baseline.
    fn publish_validated_stream_to_stage(
        &self,
        spool: &crate::native_evidence::FullDocumentSpool,
        capture: &crate::capture::Capture,
        stage: &StagedIndex,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        leader.belongs_to(&self.identity)?;
        leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
        stage.verify_path()?;
        self.identity.verify()?;
        let index_dir = self.roots.index_dir(&self.identity);
        let stage_id = stage
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("index.db.tmp-"));
        ensure!(
            stage.path.parent() == Some(index_dir.as_path())
                && stage_id.is_some_and(|value| uuid::Uuid::parse_str(value).is_ok()),
            "unsafe_index: stage is outside the managed index directory"
        );
        let bootstrap = open_index(&stage.path, false)?;
        let version: u32 = bootstrap.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version == DATABASE_SCHEMA_VERSION,
            "incompatible_index: stage is not an unpublished bootstrap"
        );
        validate_v8_bootstrap(&bootstrap)?;
        let expected = self.recovery_baseline(&bootstrap)?;
        ensure!(
            expected.pin().is_some_and(|pin| pin.index_revision == 0) && expected.compatible,
            "incompatible_index: invalid private stage baseline"
        );
        drop(bootstrap);
        stage.verify_path()?;
        leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
        self.publish_validated_stream_target(
            spool,
            capture,
            leader,
            expected,
            PublicationTarget::Stage(stage),
            cancel,
            None,
            |_, _| Ok(()),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_validated_stream_target(
        &self,
        spool: &crate::native_evidence::FullDocumentSpool,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        expected: RecoveryBaseline,
        target: PublicationTarget<'_>,
        cancel: &CancelFlag,
        semantic: Option<(
            &crate::semantic_evidence::SemanticPublication,
            &crate::semantic_evidence::CapturedComponents,
        )>,
        during_tx: impl FnMut(PublishStage, &Connection) -> Result<()>,
    ) -> Result<IndexPin> {
        capture.verify(cancel)?;
        ensure!(
            capture.graph_projection_count() == 1,
            "native_evidence_required: capture must have one graph projection"
        );
        ensure!(
            capture.source_operations.len() == capture.files.len()
                && capture
                    .source_operations
                    .values()
                    .all(|counts| counts.opens == 1
                        && counts.complete_reads == 1
                        && counts.hashes == 1),
            "native_evidence_required: each source must open, read, and hash once"
        );
        ensure!(
            spool.len() == spool.header().revision.documents.len()
                && spool.len() == capture.files.len(),
            "native_evidence_required: streamed document count mismatch"
        );
        let documents: BTreeMap<_, _> = spool
            .header()
            .revision
            .documents
            .iter()
            .map(|doc| (doc.key.path.as_str(), doc))
            .collect();
        ensure!(
            documents.len() == spool.len(),
            "native_evidence_required: duplicate revision document path"
        );
        // The full source set remains borrowed from the attested capture. Only
        // each one-document graph decoded from the spool owns a source copy.
        let graph = Graph {
            stats: spool.stats().clone(),
            diagnostics: spool.diagnostics().to_vec(),
            ..Graph::default()
        };
        let mut unique_paths = BTreeSet::new();
        for source in &capture.files {
            ensure!(
                unique_paths.insert(source.path.as_str()),
                "native_evidence_required: duplicate captured source path"
            );
            let doc = documents
                .get(source.path.as_str())
                .context("native document missing captured source")?;
            ensure!(
                doc.key.language == source.language
                    && doc.content_hash == source.hash
                    && doc.byte_length == source.text.len(),
                "native document bytes differ from captured graph"
            );
        }
        // The coordinator attests this committed pin once before it returns.
        self.publish_inner_checked_target_with_local(
            (&graph, capture, spool.header()),
            leader,
            PublicationPlan {
                expected: ExpectedPublication::Recovery(Box::new(expected)),
                target,
            },
            cancel,
            (256 * 1024 * 1024 + 16 * 1024, None, Some(spool)),
            during_tx,
            semantic,
        )
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_private_stage_semantic_for_tests(
        &self,
        options: &crate::indexer::IndexOptions,
        capture: &crate::capture::Capture,
        semantic: &crate::semantic_evidence::SemanticPublication,
        captured: &crate::semantic_evidence::CapturedComponents,
        cancel: &CancelFlag,
        fail_before_commit: bool,
    ) -> Result<PrivateStageObservation> {
        self.publish_private_stage_for_tests(
            options,
            capture,
            Some((semantic, captured)),
            cancel,
            fail_before_commit,
        )
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_private_stage_native_for_tests(
        &self,
        options: &crate::indexer::IndexOptions,
        capture: &crate::capture::Capture,
        cancel: &CancelFlag,
    ) -> Result<PrivateStageObservation> {
        self.publish_private_stage_for_tests(options, capture, None, cancel, false)
    }

    #[cfg(feature = "test-support")]
    fn publish_private_stage_for_tests(
        &self,
        options: &crate::indexer::IndexOptions,
        capture: &crate::capture::Capture,
        semantic: Option<(
            &crate::semantic_evidence::SemanticPublication,
            &crate::semantic_evidence::CapturedComponents,
        )>,
        cancel: &CancelFlag,
        fail_before_commit: bool,
    ) -> Result<PrivateStageObservation> {
        let use_guard = self.roots.index_use_exclusive_existing(&self.identity)?;
        let leader = self
            .roots
            .writer_under_exclusive(self.identity.clone(), use_guard)?;
        let stage = self.create_staged_index(&leader, true)?;
        let result = (|| {
            let spool = crate::native_evidence::spool_full_capture(
                options,
                capture,
                Path::new(&self.workspace_root),
                self.root_id(),
                cancel,
                &|_| {},
                |_, _| {},
                &self.native_spool_dir()?,
            )?;
            let bootstrap = open_index(&stage.path, false)?;
            validate_v8_bootstrap(&bootstrap)?;
            let baseline = self.recovery_baseline(&bootstrap)?;
            drop(bootstrap);
            let published = self.publish_validated_stream_target(
                &spool,
                capture,
                &leader,
                baseline,
                PublicationTarget::Stage(&stage),
                cancel,
                semantic,
                |point, _| {
                    ensure!(
                        !fail_before_commit || !matches!(point, PublishStage::BeforeCommit),
                        "injected semantic stage failure"
                    );
                    Ok(())
                },
            );
            if let Err(error) = published {
                if fail_before_commit {
                    let staged = open_index(&stage.path, false)?;
                    for table in [
                        "native_revisions",
                        "revision_documents",
                        "semantic_coverage",
                        "semantic_references",
                    ] {
                        let count: i64 = staged.query_row(
                            &format!("SELECT count(*) FROM {table}"),
                            [],
                            |row| row.get(0),
                        )?;
                        ensure!(
                            count == 0,
                            "failed staged transaction retained {table} rows"
                        );
                    }
                }
                return Err(error);
            }
            let pin = published?;
            self.validate_replacement_index(&stage.path, &stage, &leader, pin)?;
            let staged = open_index(&stage.path, false)?;
            let key = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
            let snapshot = read_semantic(&staged, &key)?;
            let (source_set_id, native_revision_id, toolchain_hash, config_hash, dependency_hash):
                (String, String, String, String, String) = staged.query_row(
                "SELECT source_set_id,native_revision_id,toolchain_hash,config_hash,dependency_hash FROM native_revisions WHERE id=?1",
                [&key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )?;
            let documents = staged
                .prepare(
                    "SELECT m.language,m.path,d.content_hash,d.byte_length
                 FROM revision_documents m JOIN document_versions d ON d.vid=m.vid
                 WHERE m.revision_id=?1 ORDER BY m.language,m.path",
                )?
                .query_map([&key], |row| {
                    Ok(crate::native_evidence::Document {
                        key: crate::native_evidence::DocumentKey {
                            source_set_id: source_set_id.clone(),
                            language: row.get(0)?,
                            path: row.get(1)?,
                        },
                        revision_id: native_revision_id.clone(),
                        content_hash: row.get(2)?,
                        byte_length: row.get::<_, i64>(3)? as usize,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let selected = ReadRevision::new(pin, key);
            let mut declarations = vec![];
            let mut calls = vec![];
            for document in &documents {
                let scope = v8_native_scope_for(&staged, &document.key, &selected)?
                    .context("staged native document missing")?;
                declarations.extend(read_native_declarations_v8(&staged, &scope, None, true)?);
                calls.extend(read_native_calls_v8(&staged, &scope, None)?);
            }
            let revision = crate::native_evidence::Revision {
                id: native_revision_id,
                source_set_id,
                documents,
                toolchain_hash,
                config_hash,
                dependency_hash,
            };
            Ok(PrivateStageObservation {
                pin,
                semantic: snapshot,
                native_revision: revision,
                declarations,
                calls,
            })
        })();
        self.cleanup_failed_staged_index(stage, Some(&leader))?;
        result
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_stream_native_only_for_tests(
        &self,
        options: &crate::indexer::IndexOptions,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let spool = crate::native_evidence::spool_full_capture(
            options,
            capture,
            Path::new(&self.workspace_root),
            self.root_id(),
            cancel,
            &|_| {},
            |_, _| {},
            &self.native_spool_dir()?,
        )?;
        self.publish_validated_stream_recovery(
            &spool,
            capture,
            leader,
            self.publication_index_baseline()?,
            cancel,
        )
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn publish_stream_semantic_for_tests(
        &self,
        options: &crate::indexer::IndexOptions,
        capture: &crate::capture::Capture,
        semantic: &crate::semantic_evidence::SemanticPublication,
        captured: &crate::semantic_evidence::CapturedComponents,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        fail_before_commit: bool,
    ) -> Result<IndexPin> {
        let spool = crate::native_evidence::spool_full_capture(
            options,
            capture,
            Path::new(&self.workspace_root),
            self.root_id(),
            cancel,
            &|_| {},
            |_, _| {},
            &self.native_spool_dir()?,
        )?;
        self.publish_validated_stream_target(
            &spool,
            capture,
            leader,
            self.publication_index_baseline()?,
            PublicationTarget::Live,
            cancel,
            Some((semantic, captured)),
            |stage, _| {
                ensure!(
                    !fail_before_commit || !matches!(stage, PublishStage::BeforeCommit),
                    "injected semantic stream failure"
                );
                Ok(())
            },
        )
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_native_recovery_selective_for_tests(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let baseline = self.publication_index_baseline()?;
        self.publish_native_recovery_selective(graph, capture, native, leader, baseline, cancel)
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn publish_selective_semantic_for_tests(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        semantic: &crate::semantic_evidence::SemanticPublication,
        captured: &crate::semantic_evidence::CapturedComponents,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        fail_before_commit: bool,
    ) -> Result<IndexPin> {
        self.validate_native_bundle_with_mode(graph, capture, native, cancel, true)?;
        self.publish_inner_checked_target_with_local(
            (graph, capture, native),
            leader,
            PublicationPlan {
                expected: ExpectedPublication::Recovery(Box::new(
                    self.publication_index_baseline()?,
                )),
                target: PublicationTarget::Live,
            },
            cancel,
            (256 * 1024 * 1024 + 16 * 1024, None, None),
            |stage, _| {
                ensure!(
                    !fail_before_commit || !matches!(stage, PublishStage::BeforeCommit),
                    "injected semantic selective failure"
                );
                Ok(())
            },
            Some((semantic, captured)),
        )
    }

    /// A document-local native publication, which rejoins selected batches through the
    /// witness path where it applies.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_local_for_tests(
        &self,
        options: &crate::indexer::IndexOptions,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let expected = self.publication_index_baseline()?;
        let prepared = self
            .prepare_local_revision(capture, &expected, cancel)?
            .context("document-local publication unavailable")?;
        let changed = crate::indexer::project_native_documents(
            options,
            capture,
            &prepared.native,
            &prepared.changed_paths,
            cancel,
            &|_| {},
        )?;
        let graph = self.compose_local_revision(capture, &prepared, changed, cancel)?;
        self.publish_local_native_recovery(&graph, capture, prepared, leader, expected, cancel)
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn publish_local_oracle_mismatch_for_tests(
        &self,
        options: &crate::indexer::IndexOptions,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let expected = self.publication_index_baseline()?;
        ensure!(
            expected.compatible,
            "oracle fixture baseline is incompatible"
        );
        ensure!(
            self.disposition() != RecoveryDisposition::Rebuild,
            "oracle fixture store requires rebuild"
        );
        ensure!(
            self.selected_links_for_reconciliation(
                expected.pin().context("oracle fixture missing pin")?
            )?,
            "oracle fixture has no selected raw capture"
        );
        let prepared = self
            .prepare_local_revision(capture, &expected, cancel)?
            .context("document-local publication unavailable")?;
        let changed = crate::indexer::project_native_documents(
            options,
            capture,
            &prepared.native,
            &prepared.changed_paths,
            cancel,
            &|_| {},
        )?;
        let graph = self.compose_local_revision(capture, &prepared, changed, cancel)?;
        TEST_REJOIN_ORACLE_MISMATCH.set(true);
        let result =
            self.publish_local_native_recovery(&graph, capture, prepared, leader, expected, cancel);
        TEST_REJOIN_ORACLE_MISMATCH.set(false);
        result
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn publish_local_semantic_for_tests(
        &self,
        options: &crate::indexer::IndexOptions,
        capture: &crate::capture::Capture,
        semantic: &crate::semantic_evidence::SemanticPublication,
        captured: &crate::semantic_evidence::CapturedComponents,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        fail_before_commit: bool,
    ) -> Result<IndexPin> {
        let expected = self.publication_index_baseline()?;
        let prepared = self
            .prepare_local_revision(capture, &expected, cancel)?
            .context("document-local publication unavailable")?;
        let changed = crate::indexer::project_native_documents(
            options,
            capture,
            &prepared.native,
            &prepared.changed_paths,
            cancel,
            &|_| {},
        )?;
        let graph = self.compose_local_revision(capture, &prepared, changed, cancel)?;
        self.publish_local_native_recovery_with_semantic(
            &graph,
            capture,
            prepared,
            leader,
            expected,
            cancel,
            Some((semantic, captured)),
            fail_before_commit,
        )
    }

    #[allow(dead_code)] // Test seam for the original full-artifact selective rollback matrix.
    pub(crate) fn publish_native_recovery_selective(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        leader: &topology::WriterGuard,
        expected: RecoveryBaseline,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        self.publish_native_expected(
            graph,
            capture,
            native,
            leader,
            ExpectedPublication::Recovery(Box::new(expected)),
            cancel,
            true,
        )
    }
    pub(crate) fn publish_local_native_recovery(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        prepared: LocalPrepared,
        leader: &topology::WriterGuard,
        expected: RecoveryBaseline,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        self.publish_local_native_recovery_with_semantic(
            graph, capture, prepared, leader, expected, cancel, None, false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_local_native_recovery_with_semantic(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        prepared: LocalPrepared,
        leader: &topology::WriterGuard,
        expected: RecoveryBaseline,
        cancel: &CancelFlag,
        semantic: Option<(
            &crate::semantic_evidence::SemanticPublication,
            &crate::semantic_evidence::CapturedComponents,
        )>,
        fail_before_commit: bool,
    ) -> Result<IndexPin> {
        ensure!(
            capture.files == graph.files && expected.pin() == Some(prepared.prior_pin),
            "revision conflict: changed-only graph or expected head differs"
        );
        capture.claim_graph_projection()?;
        ensure!(
            capture.graph_projection_count() == 1
                && capture.source_operations.len() == capture.files.len()
                && capture
                    .source_operations
                    .values()
                    .all(|ops| ops.opens == 1 && ops.complete_reads == 1 && ops.hashes == 1),
            "native_evidence_required: incomplete selected capture"
        );
        let mut native = prepared.native.clone();
        if semantic.is_some() {
            // Reused versions belong to the selected, previously validated pin.
            // Read their measured rows through that pin, not from the changed-only
            // artifact or from a newly fabricated measurement.
            self.native_at_reconcile(prepared.prior_pin, |db, selected| {
                for document in &native.revision.documents {
                    if prepared.changed_paths.contains(&document.key.path) {
                        continue;
                    }
                    let scope = v8_native_scope_for(db, &document.key, selected)?
                        .context("reused native document missing from selected pin")?;
                    ensure!(
                        scope.version_id == prepared.prior_ids[&document.key.path].version_id,
                        "reused native version differs from selected pin"
                    );
                    let mut coverage = read_native_coverage_v8(db, &scope)?;
                    coverage.revision_id.clone_from(&native.revision.id);
                    native.coverage.push(coverage);
                    let mut declarations = read_native_declarations_v8(db, &scope, None, true)?;
                    for declaration in &mut declarations {
                        declaration.revision_id.clone_from(&native.revision.id);
                    }
                    native.declarations.extend(declarations);
                    let mut calls = read_native_calls_v8(db, &scope, None)?;
                    for call in &mut calls {
                        call.revision_id.clone_from(&native.revision.id);
                    }
                    native.calls.extend(calls);
                    let mut regions = read_native_control_regions_v8(db, &scope, None)?;
                    for region in &mut regions {
                        region.revision_id.clone_from(&native.revision.id);
                    }
                    native.control_regions.extend(regions);
                    // Native-v9 candidates and reports retain their measured inventory;
                    // only their containing revision changes for semantic validation.
                    let mut references = read_native_references(db, &scope, None)?;
                    for reference in &mut references {
                        reference.revision_id.clone_from(&native.revision.id);
                    }
                    native.references.extend(references);
                    let mut report = read_native_capability_report(db, &scope)?;
                    report.revision_id.clone_from(&native.revision.id);
                    native.capability_reports.push(report);
                    native.search_cards.extend(search::read_cards(
                        db,
                        scope.vid,
                        &document.key.path,
                    )?);
                }
                Ok(())
            })?;
        } else {
            for file in &capture.files {
                if prepared.changed_paths.contains(&file.path) {
                    continue;
                }
                let mut coverage = prepared
                    .prior_coverage
                    .get(&file.path)
                    .context("reused document coverage missing from selected head")?
                    .clone();
                coverage.revision_id.clone_from(&native.revision.id);
                native.coverage.push(coverage);
            }
        }
        let order: BTreeMap<_, _> = native
            .revision
            .documents
            .iter()
            .enumerate()
            .map(|(ordinal, document)| (document.key.path.as_str(), ordinal))
            .collect();
        native
            .coverage
            .sort_by_key(|coverage| order.get(coverage.document_path.as_str()).copied());
        self.publish_inner_checked_target_with_local(
            (graph, capture, &native),
            leader,
            PublicationPlan {
                expected: ExpectedPublication::Recovery(Box::new(expected)),
                target: PublicationTarget::Live,
            },
            cancel,
            (256 * 1024 * 1024 + 16 * 1024, Some(&prepared), None),
            |stage, _| {
                ensure!(
                    !fail_before_commit || !matches!(stage, PublishStage::BeforeCommit),
                    "injected semantic local failure"
                );
                Ok(())
            },
            semantic,
        )
    }

    #[allow(clippy::too_many_arguments)] // The selective proof is internal to this publication seam.
    fn publish_native_expected(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        leader: &topology::WriterGuard,
        expected: ExpectedPublication,
        cancel: &CancelFlag,
        selective: bool,
    ) -> Result<IndexPin> {
        self.validate_native_bundle_with_mode(graph, capture, native, cancel, selective)?;
        self.publish_inner_expected(graph, capture, native, leader, expected, cancel)
    }
    fn validate_native_bundle(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        cancel: &CancelFlag,
    ) -> Result<()> {
        self.validate_native_bundle_with_mode(graph, capture, native, cancel, false)
    }
    fn validate_native_bundle_with_mode(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        cancel: &CancelFlag,
        selective: bool,
    ) -> Result<()> {
        if selective {
            native.validate_selective(
                capture,
                Path::new(&self.workspace_root),
                &self.identity.record_id,
                cancel,
            )?;
        } else {
            native.validate(
                capture,
                Path::new(&self.workspace_root),
                &self.identity.record_id,
                cancel,
            )?;
        }
        self.validate_bundle_projection(graph, capture, native, cancel)
    }
    fn validate_bundle_projection(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        cancel: &CancelFlag,
    ) -> Result<()> {
        ensure!(
            capture.graph_projection_count() == 1,
            "native_evidence_required: capture must have one graph projection"
        );
        ensure!(
            capture.source_operations.len() == capture.files.len()
                && capture
                    .source_operations
                    .values()
                    .all(|counts| counts.opens == 1
                        && counts.complete_reads == 1
                        && counts.hashes == 1),
            "native_evidence_required: each source must open, read, and hash once"
        );
        crate::indexer::validate_native_graph(graph, capture, native, cancel)?;
        Ok(())
    }
    /// Publish to an unpublished private bootstrap while the caller retains EX use.
    /// This stages the full pair without admitting evidence from the corrupt live DB.
    #[allow(dead_code)] // The exceptional recovery entry point is wired after this scoped seam.
    fn publish_native_to_stage(
        &self,
        bundle: (
            &Graph,
            &crate::capture::Capture,
            &crate::native_evidence::Artifact,
        ),
        stage: &StagedIndex,
        leader: &topology::WriterGuard,
        cancel: &CancelFlag,
        during_tx: impl FnMut(PublishStage, &Connection) -> Result<()>,
    ) -> Result<IndexPin> {
        let (graph, capture, native) = bundle;
        leader.belongs_to(&self.identity)?;
        leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
        stage.verify_path()?;
        self.identity.verify()?;
        let index_dir = self.roots.index_dir(&self.identity);
        let stage_id = stage
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("index.db.tmp-"));
        ensure!(
            stage.path.parent() == Some(index_dir.as_path())
                && stage_id.is_some_and(|value| uuid::Uuid::parse_str(value).is_ok()),
            "unsafe_index: stage is outside the managed index directory"
        );
        let bootstrap = open_index(&stage.path, false)?;
        let version: u32 = bootstrap.pragma_query_value(None, "user_version", |row| row.get(0))?;
        ensure!(
            version == DATABASE_SCHEMA_VERSION,
            "incompatible_index: stage is not an unpublished bootstrap"
        );
        // The stage must be the exact empty v8 bootstrap we just created.
        // A published or partially written stage is never a recovery source.
        validate_v8_bootstrap(&bootstrap)?;
        let expected = self.recovery_baseline(&bootstrap)?;
        ensure!(
            expected.pin().is_some_and(|pin| pin.index_revision == 0) && expected.compatible,
            "incompatible_index: invalid private stage baseline"
        );
        drop(bootstrap);
        stage.verify_path()?;
        leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
        self.validate_native_bundle(graph, capture, native, cancel)?;
        self.publish_inner_checked_target(
            bundle,
            leader,
            PublicationPlan {
                expected: ExpectedPublication::Recovery(Box::new(expected)),
                target: PublicationTarget::Stage(stage),
            },
            cancel,
            256 * 1024 * 1024 + 16 * 1024,
            during_tx,
        )
    }
    fn publish_inner_expected(
        &self,
        graph: &Graph,
        capture: &crate::capture::Capture,
        native: &crate::native_evidence::Artifact,
        leader: &topology::WriterGuard,
        expected: ExpectedPublication,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        self.publish_inner_checked_expected(
            (graph, capture, native),
            leader,
            expected,
            cancel,
            256 * 1024 * 1024 + 16 * 1024,
            |_, _| Ok(()),
        )
    }

    // Admission/read parity: only publish SourceFile JSON that pinned reads can
    // select under the same encoded-byte ceiling. This inspects the immutable
    // captured graph; it neither opens source files nor reprojects the graph.
    fn enforce_selected_source_admission(
        graph: &Graph,
        files: &[SourceFile],
        max_graph_json_bytes: usize,
    ) -> Result<()> {
        fn encoded_len<T: Serialize>(value: &T, max: usize) -> Result<usize> {
            let mut sink = EncodedSourceBudget {
                bytes: 0,
                max_bytes: max,
            };
            serde_json::to_writer(&mut sink, value)?;
            Ok(sink.bytes)
        }
        let mut selected: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
        for file in files {
            let raw = file.text.len();
            ensure!(
                raw <= 256 * 1024 * 1024,
                "incompatible_index: selected source byte budget exceeded before publication"
            );
            let envelope =
                max_graph_json_bytes.min(raw.saturating_mul(6).saturating_add(16 * 1024));
            encoded_len(file,envelope).map_err(|e|anyhow::anyhow!(
                "incompatible_index: selected source byte budget exceeded before publication: {e}"))?;
            ensure!(
                selected.insert(&file.path, (raw, 0)).is_none(),
                "incompatible_index: duplicate captured graph source"
            );
        }
        fn account<T: Serialize>(
            selected: &mut BTreeMap<&str, (usize, usize)>,
            path: &str,
            metadata_bytes: usize,
            value: &T,
        ) -> Result<()> {
            let (raw, total) = selected
                .get_mut(path)
                .context("incompatible_index: graph row without captured source")?;
            let row_cap = raw
                .saturating_mul(6)
                .saturating_add(16 * 1024)
                .min(256 * 1024 * 1024 + 16 * 1024);
            let bytes=encoded_len(value,row_cap).map_err(|e|anyhow::anyhow!(
                "incompatible_index: selected graph row byte budget exceeded before publication: {e}"))?;
            let row_bytes = metadata_bytes.checked_add(bytes).context(
                "incompatible_index: selected graph row byte budget exceeded before publication",
            )?;
            *total = total.checked_add(row_bytes).context(
                "incompatible_index: selected graph row byte budget exceeded before publication",
            )?;
            let total_cap = raw
                .saturating_mul(64)
                .saturating_add(256 * 1024)
                .min(512 * 1024 * 1024);
            ensure!(
                row_bytes <= row_cap && *total <= total_cap,
                "incompatible_index: selected graph row byte budget exceeded before publication"
            );
            Ok(())
        }
        for n in &graph.nodes {
            account(
                &mut selected,
                &n.path,
                n.id.len()
                    .saturating_add(n.name.len())
                    .saturating_add(n.path.len()),
                n,
            )?;
        }
        for c in &graph.calls {
            account(
                &mut selected,
                &c.path,
                c.id.len()
                    .saturating_add(c.caller.len())
                    .saturating_add(c.path.len()),
                c,
            )?;
        }
        for r in &graph.regions {
            account(
                &mut selected,
                &r.path,
                r.id.len()
                    .saturating_add(r.owner.len())
                    .saturating_add(r.path.len()),
                r,
            )?;
        }
        Ok(())
    }
    // Private transaction seam used by the in-module rollback tests. Normal callers
    // always pass a no-op; no SQL-fault control is exposed to API or CLI clients.
    #[cfg(test)]
    fn publish_inner_checked(
        &self,
        bundle: (
            &Graph,
            &crate::capture::Capture,
            &crate::native_evidence::Artifact,
        ),
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
        during_tx: impl FnMut(PublishStage, &Connection) -> Result<()>,
    ) -> Result<IndexPin> {
        self.publish_inner_checked_with_source_cap(
            bundle,
            leader,
            expected_revision,
            cancel,
            256 * 1024 * 1024 + 16 * 1024,
            during_tx,
        )
    }
    // This test-only injection exercises the production admission path with a
    // small graph-JSON cap, without creating a 256MiB escaped source fixture.
    #[cfg(test)]
    fn publish_inner_checked_with_source_cap(
        &self,
        bundle: (
            &Graph,
            &crate::capture::Capture,
            &crate::native_evidence::Artifact,
        ),
        leader: &topology::WriterGuard,
        expected_revision: IndexPin,
        cancel: &CancelFlag,
        max_graph_json_bytes: usize,
        during_tx: impl FnMut(PublishStage, &Connection) -> Result<()>,
    ) -> Result<IndexPin> {
        self.publish_inner_checked_expected(
            bundle,
            leader,
            ExpectedPublication::Pin(expected_revision),
            cancel,
            max_graph_json_bytes,
            during_tx,
        )
    }
    fn publish_inner_checked_expected(
        &self,
        bundle: (
            &Graph,
            &crate::capture::Capture,
            &crate::native_evidence::Artifact,
        ),
        leader: &topology::WriterGuard,
        expected: ExpectedPublication,
        cancel: &CancelFlag,
        max_graph_json_bytes: usize,
        during_tx: impl FnMut(PublishStage, &Connection) -> Result<()>,
    ) -> Result<IndexPin> {
        self.publish_inner_checked_target(
            bundle,
            leader,
            PublicationPlan {
                expected,
                target: PublicationTarget::Live,
            },
            cancel,
            max_graph_json_bytes,
            during_tx,
        )
    }
    /// A publishing writer may read its prior snapshot after begin_writer_publication
    /// has made public reads not-ready. This private snapshot never serves clients.
    fn with_prior_publication_snapshot<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        (|| {
            let db = self.cache()?;
            db.execute_batch("BEGIN DEFERRED")?;
            begin_read_snapshot(
                &db,
                &self.roots.index_db(&self.identity),
                Duration::from_secs(5),
            )?;
            let baseline = self.recovery_baseline_preflight(&db)?;
            ensure!(
                baseline.compatible && baseline.pin().is_some_and(|p| p.index_revision > 0),
                "incompatible_index: prior publication is not a current head"
            );
            let result = read(&db);
            let identity = self.identity.verify();
            db.execute_batch("ROLLBACK")?;
            identity?;
            result
        })()
        .map_err(|error| self.report_index_failure(error))
    }

    /// Read a prior immutable native version without running the native parser.
    /// Its source bytes must be exactly the newly admitted immutable source. The
    /// normalized rows are checked against the witness written at full admission.
    fn selected_reusable_native(
        &self,
        db: &Connection,
        selected: &ReadRevision,
        file: &SourceFile,
    ) -> Result<crate::native_evidence::SelectedDocument> {
        use crate::native_evidence::{
            Document, DocumentKey, Producer, Provenance, Revision, SelectedDocument, SourceSet,
        };
        use sha2::{Digest, Sha256};
        let key = DocumentKey {
            source_set_id: format!("source-set:v1:{}", self.root_id()),
            language: file.language.clone(),
            path: file.path.clone(),
        };
        let scope = v8_native_scope_for(db, &key, selected)?.ok_or_else(|| {
            SelectedIntegrity("incompatible_index: reusable native version absent".into())
        })?;
        let (hash, length, bytes, context, producer_id, producer_version, witness): (
            String,
            i64,
            Vec<u8>,
            String,
            String,
            String,
            String,
        ) = db.query_row(
            "SELECT d.content_hash,d.byte_length,d.source_bytes,d.extraction_context,
                d.producer_id,d.producer_version,d.native_witness FROM document_versions d
                WHERE d.vid=?1 AND d.language=?2 AND d.path=?3",
            params![scope.vid, key.language, key.path],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )?;
        selected_ensure!(
            hash == file.hash
                && bytes == file.text.as_bytes()
                && length == bytes.len() as i64
                && hash == hex::encode(Sha256::digest(&bytes))
                && context
                    == crate::native_evidence::declared_selected_extraction_context(
                        &producer_id,
                        &producer_version,
                        &file.language,
                    )?
                && producer_id == scope.producer_id
                && producer_version == crate::native_evidence::NATIVE_VERSION,
            "incompatible_index: reusable native source/context differs from capture"
        );
        let (_origin, kind, position_encoding): (String, String, String) = db.query_row(
            "SELECT executable_hash,kind,position_encoding FROM native_producers
                WHERE id=?1 AND version=?2",
            params![producer_id, producer_version],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let executable_hash = selected_producer_hash(db, selected)?;
        let producer = Producer { id:producer_id,version:producer_version,executable_hash,
            kind,languages:db.prepare("SELECT language FROM native_producer_languages WHERE producer_id=?1 AND producer_version=?2 ORDER BY ordinal")?
                .query_map(params![scope.producer_id,crate::native_evidence::NATIVE_VERSION],|r|r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,position_encoding };
        let source_set = SourceSet { id:key.source_set_id.clone(),root_id:self.root_id().to_owned(),
            languages:db.prepare("SELECT language FROM native_source_set_languages WHERE source_set_id=?1 ORDER BY ordinal")?
                .query_map([&key.source_set_id],|r|r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()?,
            dependencies:db.prepare("SELECT dependency_id FROM native_source_set_dependencies WHERE source_set_id=?1 ORDER BY ordinal")?
                .query_map([&key.source_set_id],|r|r.get(0))?.collect::<rusqlite::Result<Vec<_>>>()? };
        let (toolchain_hash,config_hash,dependency_hash):(String,String,String)=db.query_row(
            "SELECT toolchain_hash,config_hash,dependency_hash FROM native_revisions WHERE id=?1",
            [&selected.key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let document = Document {
            key: key.clone(),
            revision_id: scope.native_revision_id.clone(),
            content_hash: hash,
            byte_length: bytes.len(),
        };
        let revision = Revision {
            id: scope.native_revision_id.clone(),
            source_set_id: key.source_set_id.clone(),
            documents: vec![document.clone()],
            toolchain_hash,
            config_hash,
            dependency_hash,
        };
        let coverage = read_native_coverage_v8(db, &scope)?;
        let mut declarations = read_native_declarations_v8(db, &scope, None, true)?;
        // Normalized SQL keys are syntax-ID ordered. Projection expects the
        // producer's preorder (outer ranges and ancestors before children).
        declarations.sort_by_key(|d| {
            (
                d.range.start,
                std::cmp::Reverse(d.range.end),
                d.ancestors.len(),
            )
        });
        let calls = read_native_calls_v8(db, &scope, None)?;
        let control_regions = read_native_control_regions_v8(db, &scope, None)?;
        let references = read_native_references(db, &scope, None)?;
        let capability_report = read_native_capability_report(db, &scope)?;
        let observed = crate::native_evidence::document_witness(
            &document,
            &producer,
            &coverage,
            &declarations,
            &calls,
            &control_regions,
            &references,
            &capability_report,
        )?;
        selected_ensure!(
            observed == witness,
            "incompatible_index: reusable normalized native witness mismatch"
        );
        let provenance = Provenance {
            id: scope.proof_id(),
            producer_id: producer.id.clone(),
            document: key,
            revision_id: scope.native_revision_id,
            content_hash: file.hash.clone(),
            evidence_kind: "measuredSyntax".into(),
            basis: None,
            freshness: "fresh".into(),
            derived_from: None,
        };
        Ok(SelectedDocument {
            producer,
            source_set,
            revision,
            document,
            coverage,
            provenance,
            declarations,
            calls,
            control_regions,
            references,
            capability_report,
            search_cards: search::read_cards(db, scope.vid, &file.path)?,
        })
    }

    /// Option C: admit a proven single-document body edit without decoding any
    /// unchanged occurrence or projection row. IDs must come from the selected
    /// same-generation head, never an orphan or a matching historical version.
    /// A leader Serve can advance an unchanged, already-validated head without
    /// re-extracting native facts. This is deliberately stricter than stat
    /// equality: Capture::admit has freshly read and SHA-256 hashed every source,
    /// and the indexer's saved content hashes identify the validated versions.
    /// Stored source bytes are not re-attested by these UNCHANGED writer paths.
    /// A leader's final full inventory may avoid a redundant publication only
    /// when the freshly hashed capture is still exactly the selected head.
    /// This read-only guard is not an explicit-request completion path.
    pub(crate) fn selected_capture_unchanged(
        &self,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        expected: &RecoveryBaseline,
        cancel: &CancelFlag,
    ) -> Result<bool> {
        if !expected.compatible
            || self.disposition() != RecoveryDisposition::Ready
            || !expected.pin().is_some_and(|pin| pin.index_revision > 0)
        {
            return Ok(false);
        }
        check_cancel(cancel)?;
        ensure!(
            capture.source_operations.len() == capture.files.len()
                && capture
                    .source_operations
                    .values()
                    .all(|ops| ops.opens == 1 && ops.complete_reads == 1 && ops.hashes == 1),
            "native_evidence_required: final inventory must freshly read and hash every admitted source"
        );
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        let db = self.cache_write()?;
        let admitted_version: i64 =
            storage_result(db.pragma_query_value(None, "data_version", |row| row.get(0)))?;
        let selected = ReadRevision::current(&db)?;
        ensure!(
            expected.pin() == Some(selected.pin),
            "revision conflict: final inventory selected head changed"
        );
        let baseline = self.recovery_baseline_preflight(&db)?;
        ensure!(
            baseline.compatible
                && baseline.pin == Some(selected.pin)
                && baseline.witness == expected.witness,
            "revision conflict: final inventory baseline changed"
        );
        validate_reconcile_inventory(&db)?;
        let executing_hash = capture
            .executable_digest(capture.executable_path())
            .context("native_evidence_required: final inventory executable not hashed")?;
        if selected_producer_hash(&db, &selected)? != executing_hash {
            return Ok(false);
        }
        let prior_options: String = db.query_row(
            "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if prior_options != json(capture.reconcile_options())? {
            return Ok(false);
        }
        let comparison = compare_capture_snapshot_for_unchanged(&db, capture, true, cancel)?;
        if comparison.examined == 0 || comparison.changed() {
            return Ok(false);
        }
        capture.verify(cancel)?;
        leader.verify()?;
        self.identity.verify()?;
        let after: i64 =
            storage_result(db.pragma_query_value(None, "data_version", |row| row.get(0)))?;
        ensure!(
            admitted_version == after
                && self.recovery_baseline_preflight(&db)?.witness == expected.witness
                && ReadRevision::current(&db)?.pin == selected.pin,
            "revision conflict: final inventory changed at cutoff"
        );
        Ok(true)
    }

    pub(crate) fn reuse_unchanged_native_head(
        &self,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        expected: &RecoveryBaseline,
        cancel: &CancelFlag,
    ) -> Result<Option<IndexPin>> {
        if !expected.compatible
            || self.disposition() != RecoveryDisposition::Ready
            || !expected.pin().is_some_and(|pin| pin.index_revision > 0)
        {
            return Ok(None);
        }
        check_cancel(cancel)?;
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        ensure!(
            capture.source_operations.len() == capture.files.len()
                && capture
                    .source_operations
                    .values()
                    .all(|ops| ops.opens == 1 && ops.complete_reads == 1 && ops.hashes == 1),
            "native_evidence_required: every unchanged source must be read and hashed"
        );
        let db = self.cache_write()?;
        let admitted_version: i64 =
            storage_result(db.pragma_query_value(None, "data_version", |row| row.get(0)))?;
        let selected = ReadRevision::current(&db)?;
        if expected.pin() != Some(selected.pin) {
            anyhow::bail!("revision conflict: Serve selected head changed");
        }
        // Public reads are deliberately blocked while a leader is publishing.
        // Apply their paired metadata/inventory checks in this private fenced
        // snapshot instead of calling read_status, which requires public-ready.
        let selected_baseline = self.recovery_baseline_preflight(&db)?;
        selected_ensure!(
            selected_baseline.compatible && selected_baseline.pin == Some(selected.pin),
            "incompatible_index: selected Serve head is not validated"
        );
        validate_reconcile_inventory(&db)?;
        let class_warning_bytes: Option<i64> = db
            .query_row(
                "SELECT length(CAST(class_warnings AS BLOB)) FROM native_revisions WHERE id=?1",
                [&selected.key],
                |row| row.get(0),
            )
            .optional()?;
        selected_ensure!(
            class_warning_bytes.is_some_and(|bytes| (0..=256 * 1024).contains(&bytes)),
            "incompatible_index: selected class catalog budget exceeded or missing"
        );
        let executing_hash = capture
            .executable_digest(capture.executable_path())
            .context("native_evidence_required: executable was not hashed at admission")?;
        // Decision 0005 (2026-10 amendment): a rebuilt executable keeps the
        // selected facts only while its build-time extractor producer hash
        // equals the one the selected head recorded. The reuse comparison
        // below includes that hash and leaves out the executable observation;
        // a changed or unrecorded hash is a change, so FULL measurement wins.
        let drift = selected_producer_hash(&db, &selected)? != executing_hash;
        let prior_options: String = db.query_row(
            "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if prior_options != json(capture.reconcile_options())? {
            return Ok(None);
        }
        let comparison = compare_capture_snapshot_for_unchanged(&db, capture, false, cancel)?;
        if comparison.changed() || comparison.examined == 0 {
            // This compares the freshly hashed source content AND the captured
            // root, directory, config, selector, presentation and extractor
            // observations.
            return Ok(None);
        }
        // A rebuilt executable changes only the toolchain part of the revision
        // header (its SHA is revision provenance). Build the fresh header from
        // this capture; every other header field must equal the selected one.
        let fresh_header = if drift {
            let (source_set, header) = crate::native_evidence::captured_revision_header(
                capture,
                Path::new(&self.workspace_root),
                self.root_id(),
            )?;
            let (source_set_id, config_hash, dependency_hash, dependencies): (
                String,
                String,
                String,
                String,
            ) = db.query_row(
                "SELECT source_set_id,config_hash,dependency_hash,dependency_observations
                 FROM native_revisions WHERE id=?1",
                [&selected.key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
            if source_set_id != header.source_set_id
                || config_hash != header.config_hash
                || dependency_hash != header.dependency_hash
                || dependencies != json(&source_set.dependencies)?
            {
                return Ok(None);
            }
            Some(header)
        } else {
            None
        };
        // The same connection fences the authenticated preflight against a
        // concurrent direct SQL rewrite. The leader/use guards exclude normal
        // writers, while the metadata CAS excludes stale in-process requests.
        let after_preflight: i64 =
            storage_result(db.pragma_query_value(None, "data_version", |row| row.get(0)))?;
        ensure!(
            after_preflight == admitted_version,
            "incompatible_index: index changed during unchanged preflight"
        );
        check_cancel(cancel)?;
        leader.verify()?;
        self.identity.verify()?;
        // Decision 0006 §3 (amended 2026-10-20): an unchanged capture publishes
        // nothing. A copied revision would equal the head except for its
        // number, so the request or startup reconciliation keeps the head.
        self.ensure_not_recreate_pending()?;
        let after: i64 =
            storage_result(db.pragma_query_value(None, "data_version", |row| row.get(0)))?;
        let current = self.recovery_baseline_preflight(&db)?;
        ensure!(
            after == admitted_version
                && current.compatible
                && current.pin == expected.pin()
                && current.witness == expected.witness
                && ReadRevision::current(&db)?.pin == selected.pin,
            "revision conflict: unchanged baseline changed"
        );
        let Some(header) = fresh_header else {
            // Capture::admit already verified this fresh inventory. UNCHANGED
            // publishes nothing, so it needs no second full filesystem walk.
            return Ok(Some(selected.pin));
        };
        capture.verify(cancel)?;
        leader.verify()?;
        self.identity.verify()?;
        // Decision 0005 (2026-10 amendment): a rebuilt executable with an
        // unchanged extractor producer hash keeps every selected fact, but
        // the head must carry this executable's provenance and binding. Publish
        // one metadata-only revision: the selected manifest under a fresh
        // header (toolchain hash and native revision ID) and fresh inputs.
        let mut db = db;
        let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let locked = self.recovery_baseline_preflight(&tx)?;
        let locked_version: i64 =
            storage_result(tx.pragma_query_value(None, "data_version", |row| row.get(0)))?;
        ensure!(
            locked_version == admitted_version
                && locked.pin == expected.pin()
                && locked.witness == expected.witness
                && locked.compatible,
            "revision conflict: rebuilt-executable baseline changed"
        );
        let revision = IndexPin {
            index_generation: selected.pin.index_generation,
            index_revision: selected
                .pin
                .index_revision
                .checked_add(1)
                .context("revision overflow")?,
        };
        let revision_key = format!(
            "pin:v1:{}:{}",
            revision.index_generation, revision.index_revision
        );
        let (retained_toolchain, _, _) =
            crate::native_evidence::captured_components(capture, executing_hash)?;
        ensure!(
            header.toolchain_hash
                == crate::native_ids::digest(
                    b"",
                    &crate::native_ids::canonical(&retained_toolchain)
                ),
            "native_evidence_required: rebuilt toolchain component differs"
        );
        let copied = tx.execute(
                "INSERT INTO native_revisions SELECT ?1,source_set_id,?4,config_hash,             dependency_hash,?5,?6,dependency_observations,?7,retained_config,retained_dependencies,             reconcile_options,class_warnings,class_truncated,graph_stats,graph_diagnostics,?2             FROM native_revisions WHERE id=?3",
                params![
                    revision_key,
                    revision.index_revision as i64,
                    selected.key,
                    header.toolchain_hash,
                    header.id,
                    json(&header.documents)?,
                    json(&retained_toolchain)?
                ],
            )?;
        selected_ensure!(
            copied == 1,
            "incompatible_index: selected native header missing"
        );
        // This capture's own observations: equal to the selected ones except
        // for the executable, whose fresh SHA the new binding records.
        for (key, observation) in capture.persisted_inputs()? {
            ensure!(
                key != "__released:v1" && key != "__pending_release:v1",
                "native_evidence_required: reserved release marker"
            );
            tx.execute(
                "INSERT INTO revision_capture_inputs VALUES(?1,?2,?3)",
                params![revision_key, key, json(&observation)?],
            )?;
        }
        let copied = tx.execute(
            "INSERT INTO revision_documents SELECT ?1,language,path,vid,             gid,cid,capture_stat,coverage_requested,             coverage_selected,coverage_state,coverage_diagnostic,ordinal,class_complete             FROM revision_documents WHERE revision_id=?2",
            params![revision_key, selected.key],
        )?;
        selected_ensure!(
            copied == capture.files.len(),
            "incompatible_index: incomplete unchanged revision manifest"
        );
        tx.execute(
            "INSERT INTO semantic_revision_producers SELECT ?1,id,version FROM semantic_revision_producers WHERE revision_id=?2",
            params![revision_key, selected.key],
        )?;
        tx.execute(
            "INSERT INTO semantic_requested_producers SELECT ?1,id,version,executable_hash FROM semantic_requested_producers WHERE revision_id=?2",
            params![revision_key, selected.key],
        )?;
        tx.execute(
            "INSERT INTO semantic_requested_producer_languages SELECT ?1,id,language,ordinal FROM semantic_requested_producer_languages WHERE revision_id=?2",
            params![revision_key, selected.key],
        )?;
        copy_semantic_rows(&tx, &selected.key, &revision_key, false)?;
        let (bound_hash, binding_sha) = revision_producer_binding(&tx, &revision_key)?;
        selected_ensure!(
            bound_hash == executing_hash,
            "incompatible_index: executing producer binding changed"
        );
        tx.execute(
            "INSERT INTO revision_producer_bindings VALUES(?1,?2,?3)",
            params![revision_key, bound_hash, binding_sha],
        )?;
        let publication_time = self.retention_time()?;
        ensure!(
            publication_time.as_secs() <= 9_007_199_254_740_991,
            "invalid_supersession_time"
        );
        let timestamp = publication_time.as_millis().to_string();
        search::publish_head(&tx, &revision_key)?;
        stamp_predecessor(&tx, &selected.key, publication_time.as_secs() as i64)?;
        selected_ensure!(
            tx.execute(
                "UPDATE index_metadata SET index_revision=?1,indexed_at=?2 WHERE singleton=1",
                params![revision.index_revision as i64, timestamp],
            )? == 1,
            "incompatible_index: missing metadata head"
        );
        validate_paired_metadata(&tx, &self.identity.record_id, PairedManifestScope::Changing)?;
        check_cancel(cancel)?;
        capture.verify(cancel)?;
        leader.verify()?;
        self.identity.verify()?;
        storage_result(tx.commit())?;
        self.published_admission.invalidate();
        self.recovery_disposition
            .store(RecoveryDisposition::Ready as u8, Ordering::Release);
        self.recovery_required.store(false, Ordering::Release);
        Ok(Some(revision))
    }

    pub(crate) fn prepare_local_revision(
        &self,
        capture: &crate::capture::Capture,
        expected: &RecoveryBaseline,
        cancel: &CancelFlag,
    ) -> Result<Option<LocalPrepared>> {
        use crate::native_evidence;
        use sha2::Digest;
        if !expected.compatible
            || self.disposition() != RecoveryDisposition::Ready
            || !expected.pin().is_some_and(|pin| pin.index_revision > 0)
        {
            return Ok(None);
        }
        self.with_prior_publication_snapshot(|db| {
            let selected = ReadRevision::current(db)?;
            ensure!(expected.pin() == Some(selected.pin),
                "revision conflict: local source snapshot changed");
            // Decision 0005: changed documents may
            // reuse unchanged documents under a rebuilt executable only while
            // the extractor producer hash is unchanged. measure_persisted_change
            // compares that recorded hash; a changed one takes the full path.
            // The selected head is a prior validated publication. Do not decode
            // its unchanged fact/projection rows; the changed captured source
            // bytes are checked below.
            let previous_options: crate::indexer::ReconcileOptions = db.query_row(
                "SELECT reconcile_options FROM index_metadata WHERE singleton=1", [],
                |r| r.get::<_, String>(0))?.parse::<serde_json::Value>()
                    .and_then(serde_json::from_value).context(SelectedIntegrity("incompatible_index: stored capture decode failed".into()))?;
            let mut previous_inputs = BTreeMap::new();
            for row in db.prepare("SELECT input_key,payload FROM revision_capture_inputs WHERE revision_id=?1 ORDER BY input_key")?
                .query_map([&selected.key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (key, payload) = row?;
                selected_ensure!(previous_inputs.insert(key, serde_json::from_str(&payload).context(SelectedIntegrity("incompatible_index: stored JSON decode failed".into()))?).is_none(),
                    "incompatible_index: duplicate prior capture input");
            }
            // Presentation labels can alter unchanged graph projections. A
            // changed SCIP or presentation manifest is not a local publication.
            let current_inputs: BTreeMap<_, _> = capture.persisted_inputs()?.into_iter().collect();
            if previous_inputs.iter().filter(|(key, _)| key.starts_with("presentation-"))
                .ne(current_inputs.iter().filter(|(key, _)| key.starts_with("presentation-")))
            {
                return Ok(None);
            }
            let mut previous = Vec::new();
            let mut prior_ids = BTreeMap::new();
            let mut prior_coverage = BTreeMap::new();
            let mut prior_summaries = BTreeMap::new();
            let mut prior_class_complete = BTreeMap::new();
            let source_set_id = format!("source-set:v1:{}", self.root_id());
            let producer_id = crate::native_evidence::PRODUCER;
            let producer_version = crate::native_evidence::NATIVE_VERSION;
            // Unchanged prior documents are trusted by their stored, validated
            // content hash (owner ruling, #141): their bytes are not read,
            // re-hashed or decoded here. Only the changed document's bytes are
            // loaded and witness-checked below.
            let mut statement = db.prepare("SELECT r.source_set_id,m.path,m.language,v.content_hash,v.byte_length,NULL,
                v.extraction_context,v.producer_id,v.producer_version,v.id,
                g.graph_hash,c.content_hash,g.graph_hash,c.content_hash,
                g.class_summary_payload,m.coverage_requested,m.coverage_selected,
                m.coverage_state,m.coverage_diagnostic,m.class_complete
                FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id
                JOIN document_versions v ON v.vid=m.vid
                JOIN graph_projections g ON g.gid=m.gid
                LEFT JOIN class_projections c ON c.cid=m.cid
                WHERE m.revision_id=?1 ORDER BY m.ordinal")?;
            for row in statement.query_map([&selected.key], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?, r.get::<_, i64>(4)?, (),
                    r.get::<_, String>(6)?, r.get::<_, String>(7)?, r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?, graph_id_row(r,10)?,
                    class_id_row(r,11)?, r.get::<_, String>(12)?,
                    r.get::<_, Option<String>>(13)?, r.get::<_, Option<String>>(14)?,
                    r.get::<_, bool>(15)?, r.get::<_, bool>(16)?,
                    r.get::<_, String>(17)?, r.get::<_, Option<String>>(18)?, r.get::<_, i64>(19)?))
            })? {
                let (row_source_set_id,path,language,hash,length,(),context,id,version,version_id,
                    graph_id,class_id,graph_hash,class_hash,summary,requested,covered,state,diagnostic,class_complete)=row?;
                selected_ensure!(row_source_set_id == source_set_id,
                    "incompatible_index: prior manifest source-set identity mismatch");
                selected_ensure!(length >= 0 && hash.len() == 64,
                    "incompatible_index: prior captured source witness mismatch");
                selected_ensure!(id == producer_id && version == producer_version
                    && context == crate::native_evidence::declared_selected_extraction_context(
                        &id, &version, &language,
                    )?
                    && class_id.is_some() && class_hash.is_some(),
                    "incompatible_index: prior document producer/context missing");
                let source = SourceFile {path:path.clone(),language:language.clone(),hash:hash.clone(),text:String::new()};
                previous.push(source);
                selected_ensure!(prior_ids.insert(path.clone(), V8DocumentProjection {
                    version_id:version_id.clone(), graph_id, class_id:class_id.context(SelectedIntegrity("class projection missing".into()))?,
                    graph_hash,class_hash:class_hash.context(SelectedIntegrity("class hash missing".into()))?,
                }).is_none(),"incompatible_index: duplicate local manifest path");
                prior_coverage.insert(path.clone(), native_evidence::Coverage {
                    producer_id:producer_id.to_owned(),language:language.clone(),
                    source_set_id:source_set_id.clone(),document_path:path.clone(),
                    revision_id:String::new(),requested,selected:covered,state,diagnostic,
                    supported_roles:vec!["definition".into(),"call".into()],
                    observed_roles:Vec::new(),
                });
                selected_ensure!(matches!(class_complete, 0 | 1),
                    "incompatible_index: invalid class selection completeness");
                prior_class_complete.insert(path.clone(), class_complete == 1);
                selected_ensure!(summary.is_some() == matches!(language.as_str(), "java" | "python"),
                    "incompatible_index: class summary missing or unexpected");
                if let Some(payload) = summary {
                    prior_summaries.insert(path.clone(), serde_json::from_str(&payload).context(SelectedIntegrity("incompatible_index: stored class summary decode failed".into()))?);
                }
            }
            let Some(affected_paths) = crate::indexer::measure_persisted_change(
                &previous, &previous_options, &previous_inputs, capture)? else { return Ok(None); };
            if affected_paths.is_empty() { return Ok(None); }
            let current_paths: BTreeSet<_> = capture.files.iter().map(|f| f.path.clone()).collect();
            let changed_paths: BTreeSet<_> = affected_paths.intersection(&current_paths).cloned().collect();
            let removed_paths: BTreeSet<_> = affected_paths.difference(&current_paths).cloned().collect();
            let mut old_changed_counts = (0_usize, 0_usize, 0_usize);
            for old in previous.iter().filter(|old| affected_paths.contains(&old.path)) {
                let stored = &prior_ids[&old.path];
                // Retain the existing changed-source witness check for every
                // replaced prior document, including removed paths.
                let (length, bytes): (i64, Vec<u8>) = db.query_row(
                    "SELECT v.byte_length,v.source_bytes FROM revision_documents m
                     JOIN document_versions v ON v.vid=m.vid
                     WHERE m.revision_id=?1 AND m.path=?2 AND m.language=?3",
                    params![selected.key, old.path, old.language], |r| Ok((r.get(0)?, r.get(1)?)))?;
                selected_ensure!(length >= 0 && length as usize == bytes.len()
                    && old.hash == hex::encode(sha2::Sha256::digest(&bytes)),
                    "incompatible_index: prior captured source witness mismatch");
                let _: String = String::from_utf8(bytes).context(SelectedIntegrity("incompatible_index: stored source encoding".into()))?;
                let old_nodes: i64 = db.query_row(
                    "SELECT count(*) FROM graph_nodes WHERE gid=(SELECT gid FROM graph_projections WHERE graph_hash=?1)",
                    [&stored.graph_hash], |r| r.get(0))?;
                let old_calls: i64 = db.query_row(
                    "SELECT count(*) FROM native_version_calls WHERE vid=(SELECT vid FROM document_versions WHERE id=?1)",
                    [&stored.version_id], |r| r.get(0))?;
                let old_regions: i64 = db.query_row(
                    "SELECT count(*) FROM native_version_control_regions WHERE vid=(SELECT vid FROM document_versions WHERE id=?1)",
                    [&stored.version_id], |r| r.get(0))?;
                old_changed_counts.0 = old_changed_counts.0.checked_add(old_nodes as usize).context("invalid prior symbol count")?;
                old_changed_counts.1 = old_changed_counts.1.checked_add(old_calls as usize).context("invalid prior call count")?;
                old_changed_counts.2 = old_changed_counts.2.checked_add(old_regions as usize).context("invalid prior region count")?;
            }
            let native = native_evidence::measure_captured_documents(
                capture, Path::new(&self.workspace_root), self.root_id(), &changed_paths, cancel, |_| {})?;
            let (stats,diagnostics):(String,String)=db.query_row(
                "SELECT m.stats,m.diagnostics
                 FROM index_metadata m JOIN native_revisions r ON r.id=?1 WHERE m.singleton=1",
                [&selected.key],|r|Ok((r.get(0)?,r.get(1)?)))?;
            Ok(Some(LocalPrepared {changed_paths,removed_paths,native,prior_pin:selected.pin,
                prior_ids,prior_coverage,prior_summaries,prior_class_complete,
                old_stats:serde_json::from_str(&stats).context(SelectedIntegrity("incompatible_index: stored JSON decode failed".into()))?,old_diagnostics:serde_json::from_str(&diagnostics).context(SelectedIntegrity("incompatible_index: stored JSON decode failed".into()))?,
                old_changed_counts}))
        })
    }

    /// Compose full revision summaries from changed facts and the selected head.
    pub(crate) fn compose_local_revision(
        &self,
        capture: &crate::capture::Capture,
        prepared: &LocalPrepared,
        changed: Graph,
        cancel: &CancelFlag,
    ) -> Result<Graph> {
        ensure!(
            changed
                .files
                .iter()
                .map(|f| f.path.clone())
                .collect::<BTreeSet<_>>()
                == prepared.changed_paths,
            "selected graph must cover exactly the measured documents"
        );
        crate::indexer::validate_native_graph_records(&changed, &prepared.native, cancel)?;
        let delta = |old: usize, removed: usize, added: usize| -> Result<usize> {
            old.checked_sub(removed)
                .and_then(|n| n.checked_add(added))
                .ok_or_else(|| {
                    self.report_index_failure(
                        SelectedIntegrity("incompatible_index: invalid prior graph summary".into())
                            .into(),
                    )
                })
        };
        let (old_nodes, old_calls, old_regions) = prepared.old_changed_counts;
        let affected = |path: &str| {
            prepared.changed_paths.contains(path) || prepared.removed_paths.contains(path)
        };
        let old_parse_errors = prepared
            .old_diagnostics
            .iter()
            .filter(|d| d.code == "parse-error" && d.path.as_deref().is_some_and(affected))
            .count();
        let calls = delta(prepared.old_stats.calls, old_calls, changed.stats.calls)?;
        let stats = IndexStats {
            files: capture.files.len(),
            symbols: delta(prepared.old_stats.symbols, old_nodes, changed.stats.symbols)?,
            calls,
            regions: delta(
                prepared.old_stats.regions,
                old_regions,
                changed.stats.regions,
            )?,
            unresolved: calls,
            parse_error_files: delta(
                prepared.old_stats.parse_error_files,
                old_parse_errors,
                changed.stats.parse_error_files,
            )?,
            ..prepared.old_stats.clone()
        };
        let mut diagnostics: Vec<_> = prepared
            .old_diagnostics
            .iter()
            .filter(|d| !d.path.as_deref().is_some_and(affected))
            .cloned()
            .collect();
        diagnostics.extend(changed.diagnostics);
        let order: BTreeMap<_, _> = prepared
            .native
            .revision
            .documents
            .iter()
            .enumerate()
            .map(|(ordinal, document)| (document.key.path.as_str(), ordinal))
            .collect();
        diagnostics.sort_by_key(|d| d.path.as_deref().and_then(|path| order.get(path)).copied());
        Ok(Graph {
            files: capture.files.clone(),
            nodes: changed.nodes,
            calls: changed.calls,
            regions: changed.regions,
            diagnostics,
            stats,
            ..Graph::default()
        })
    }

    /// Produce a selectively assembled native revision only for a proved local edit
    /// (or unchanged captured sources). Other changes use the full native path.
    #[allow(dead_code)] // Test seam; the production fast path admits changed-only facts.
    pub(crate) fn prepare_local_native(
        &self,
        capture: &crate::capture::Capture,
        expected: &RecoveryBaseline,
        cancel: &CancelFlag,
    ) -> Result<Option<crate::native_evidence::Artifact>> {
        use crate::native_evidence;
        if !expected.compatible
            || self.disposition() != RecoveryDisposition::Ready
            || !expected.pin().is_some_and(|p| p.index_revision > 0)
        {
            return Ok(None);
        }
        self.with_prior_publication_snapshot(|db| {
            let selected=ReadRevision::current(db)?;
            ensure!(expected.pin()==Some(selected.pin),
                "revision conflict: prior local snapshot changed");
            // Selected native documents carry their own executable SHA, so this
            // seam assembles only under the same executable.
            let executing_hash = capture.executable_digest(capture.executable_path())
                .context("native_evidence_required: executable was not hashed at admission")?;
            if selected_producer_hash(db, &selected)? != executing_hash {
                return Ok(None);
            }
            // Actual selected reuse is authenticated by selected_reusable_native below;
            // unrelated retained versions must not be decoded.
            let previous_options:crate::indexer::ReconcileOptions = db.query_row(
                "SELECT reconcile_options FROM index_metadata WHERE singleton=1",[],
                |r|r.get::<_,String>(0))?.parse::<serde_json::Value>()
                    .and_then(serde_json::from_value).context(SelectedIntegrity("incompatible_index: stored capture decode failed".into()))?;
            let mut previous_inputs=BTreeMap::new();
            for row in db.prepare("SELECT input_key,payload FROM revision_capture_inputs WHERE revision_id=?1 ORDER BY input_key")?
                .query_map([&selected.key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {
                let (key,payload)=row?;
                selected_ensure!(previous_inputs.insert(key,serde_json::from_str(&payload).context(SelectedIntegrity("incompatible_index: stored JSON decode failed".into()))?).is_none(),
                    "incompatible_index: duplicate prior capture input");
            }
            let mut previous=vec![];
            for row in db.prepare("SELECT d.path,d.language,d.content_hash,d.byte_length,d.source_bytes
                FROM revision_documents m JOIN document_versions d ON d.vid=m.vid
                WHERE m.revision_id=?1 ORDER BY m.ordinal")?
                .query_map([&selected.key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,
                    r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,Vec<u8>>(4)?)))? {
                use sha2::{Digest,Sha256};
                let (path,language,hash,length,bytes)=row?;
                selected_ensure!(length>=0 && length as usize==bytes.len()
                    && bytes.len()<=256*1024*1024
                    && hex::encode(Sha256::digest(&bytes))==hash,
                    "incompatible_index: prior captured source witness mismatch");
                previous.push(SourceFile {path,language,hash,text:String::from_utf8(bytes).context(SelectedIntegrity("incompatible_index: stored source encoding".into()))?});
            }
            let Some(selected_paths) = crate::indexer::measure_persisted_change(
                &previous,&previous_options,&previous_inputs,capture)? else { return Ok(None); };
            let root=Path::new(&self.workspace_root);
            let mut measured=vec![];
            for file in &capture.files {
                if selected_paths.contains(&file.path) {
                    let changed=native_evidence::measure_captured_document(
                        capture,root,self.root_id(),&file.path,cancel, |_| {})?;
                    measured.push(changed);
                } else {
                    measured.push(self.selected_reusable_native(db,&selected,file).context("incompatible_index: prior native witness invalid")?);
                }
            }
            let artifact=native_evidence::assemble_selected_revision(
                capture,root,self.root_id(),measured,cancel)?;
            Ok(Some(artifact))
        })
    }

    /// R-1: a reused projection's stored row must carry the fresh projection
    /// hash. Its child rows were bound to that hash when Trellis wrote and
    /// validated them, so they are not decoded again here. A mismatch is typed
    /// index corruption.
    fn verify_reusable_projection(
        &self,
        db: &Connection,
        ids: &V8DocumentProjection,
        reuse: ReusedFamilies,
        extraction: Option<&crate::model::FileExtraction>,
    ) -> Result<()> {
        if reuse.graph {
            let (version, hash, state, class_state, stored_f):
                (String,String,String,String,Option<String>) = db.query_row(
                "SELECT v.id,g.graph_hash,g.state,g.class_extraction_state,g.class_extraction_payload
                    FROM graph_projections g JOIN document_versions v ON v.vid=g.vid
                    WHERE g.graph_hash=?1",
                [&ids.graph_hash],
                |r| Ok((r.get(0)?, r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)),
            )?;
            let expected_f = extraction.map(json).transpose()?;
            control_ensure!(
                state == "ready"
                    && stored_f == expected_f
                    && (if extraction.is_some() {
                        class_state == "ready"
                    } else {
                        class_state == "notApplicable"
                    }),
                "incompatible_index: reusable class extraction witness mismatch"
            );
            control_ensure!(
                version == ids.version_id && hash == ids.graph_hash,
                "incompatible_index: reusable graph witness mismatch"
            );
        }
        if reuse.class {
            let (graph_id, hash): (String, String) = db.query_row(
                "SELECT g.graph_hash,c.content_hash FROM class_projections c
                    JOIN graph_projections g ON g.gid=c.gid WHERE c.content_hash=?1",
                [&ids.class_hash],
                |r| Ok((graph_id_row(r, 0)?, r.get(1)?)),
            )?;
            control_ensure!(
                graph_id == ids.graph_id && hash == ids.class_hash,
                "incompatible_index: reusable class witness mismatch"
            );
        }
        Ok(())
    }

    /// Authenticate candidates in a read snapshot before entering the writer lock.
    /// A missing or corrupt prior witness is never treated as permission to reuse.
    fn preflight_local_reuse(
        &self,
        graph: &Graph,
        prepared: &LocalPrepared,
        rows: &BTreeMap<&str, PublicationRows<'_>>,
        class_complete: bool,
    ) -> Result<PreflightReuse> {
        self.with_prior_publication_snapshot(|db| {
            let selected = ReadRevision::current(db)?;
            ensure!(
                selected.pin == prepared.prior_pin,
                "revision conflict: selected local head changed"
            );
            let files: BTreeMap<_, _> = graph
                .files
                .iter()
                .map(|file| (file.path.as_str(), file))
                .collect();
            let mut families = BTreeMap::new();
            let mut existing_ids = BTreeMap::new();
            let mut count = 0;
            let mut statement = db.prepare(
                "SELECT m.path,m.language,v.id,
                g.graph_hash,c.content_hash,v.content_hash,v.extraction_context,
                v.producer_id,v.producer_version
                FROM revision_documents m JOIN document_versions v ON v.vid=m.vid
                JOIN graph_projections g ON g.gid=m.gid
                LEFT JOIN class_projections c ON c.cid=m.cid
                WHERE m.revision_id=?1 ORDER BY m.ordinal",
            )?;
            for row in statement.query_map([&selected.key], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    graph_id_row(r, 3)?,
                    class_id_row(r, 4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, String>(7)?,
                    r.get::<_, String>(8)?,
                ))
            })? {
                let (
                    path,
                    language,
                    version_id,
                    graph_id,
                    class_id,
                    hash,
                    context,
                    producer_id,
                    producer_version,
                ) = row?;
                count += 1;
                let old = prepared
                    .prior_ids
                    .get(&path)
                    .context("prior selected ID missing")?;
                selected_ensure!(
                    old.version_id == version_id
                        && old.graph_id == graph_id
                        && Some(old.class_id.as_str()) == class_id.as_deref(),
                    "incompatible_index: local selected manifest changed"
                );
                let Some(file) = files.get(path.as_str()) else {
                    selected_ensure!(
                        prepared.removed_paths.contains(&path),
                        "incompatible_index: unrecorded local removal"
                    );
                    continue;
                };
                if prepared.changed_paths.contains(&path) {
                    continue;
                }
                selected_ensure!(
                    file.language == language
                        && producer_id == prepared.native.producer.id
                        && producer_version == prepared.native.producer.version
                        && context
                            == crate::native_evidence::native_extraction_context(
                                &prepared.native.producer,
                                &language,
                                &prepared.native.revision,
                            )?,
                    "incompatible_index: local producer or extraction context changed"
                );
                {
                    selected_ensure!(
                        hash == file.hash,
                        "incompatible_index: reusable captured content hash differs"
                    );
                    let (next_class_id, next_class_hash) = if class_complete
                        && prepared.prior_class_complete.get(&path) == Some(&true)
                    {
                        (old.class_id.clone(), old.class_hash.clone())
                    } else {
                        let grouped = rows
                            .get(path.as_str())
                            .context("local class projection rows missing")?;
                        #[cfg(test)]
                        LOCAL_CLASS_PROJECTION_HASHES.with(|count| count.set(count.get() + 1));
                        v8_class_projection(&old.graph_id, grouped)
                    };
                    selected_ensure!(
                        families
                            .insert(
                                path.clone(),
                                ReusedFamilies {
                                    native: true,
                                    graph: true,
                                    class: next_class_hash == old.class_hash,
                                }
                            )
                            .is_none(),
                        "incompatible_index: duplicate reused path"
                    );
                    existing_ids.insert(
                        path,
                        V8DocumentProjection {
                            version_id,
                            graph_id,
                            class_id: next_class_id,
                            graph_hash: old.graph_hash.clone(),
                            class_hash: next_class_hash,
                        },
                    );
                }
            }
            selected_ensure!(
                count == prepared.prior_ids.len()
                    && existing_ids.len() + prepared.changed_paths.len() == files.len(),
                "incompatible_index: local selected head incomplete"
            );
            Ok(PreflightReuse {
                pin: Some(selected.pin),
                families,
                existing_ids,
            })
        })
    }

    fn preflight_reuse(
        &self,
        graph: &Graph,
        native: &crate::native_evidence::Artifact,
        extractions: &BTreeMap<String, crate::model::FileExtraction>,
        expected: &ExpectedPublication,
        rows: &BTreeMap<&str, PublicationRows<'_>>,
    ) -> Result<PreflightReuse> {
        if self.disposition() != RecoveryDisposition::Ready
            || !match expected {
                ExpectedPublication::Pin(pin) => pin.index_revision > 0,
                ExpectedPublication::Recovery(baseline) => {
                    baseline.compatible && baseline.pin().is_some_and(|p| p.index_revision > 0)
                }
            }
        {
            return Ok(PreflightReuse::default());
        }
        self.with_prior_publication_snapshot(|db| {
            let selected = ReadRevision::current(db)?;
            if !match expected {
                ExpectedPublication::Pin(pin) => *pin == selected.pin,
                ExpectedPublication::Recovery(baseline) => {
                    baseline.pin() == Some(selected.pin) && baseline.compatible
                }
            } || selected.pin.index_revision == 0
            {
                return Ok(PreflightReuse::default());
            }
            // T00 attests only versions actually reused below. Unselected
            // historical versions must not affect this fresh publication.
            let drift = selected_producer_hash(db, &selected)? != native.producer.executable_hash;
            let documents: BTreeMap<_, _> = native.revision.documents.iter()
                .map(|doc| (doc.key.path.as_str(), doc)).collect();
            let coverages: BTreeMap<_, _> = native.coverage.iter()
                .map(|coverage| (coverage.document_path.as_str(), coverage)).collect();
            let mut old = BTreeMap::new();
            let mut stmt = db.prepare(
                "SELECT m.path,v.id,g.graph_hash,
                c.content_hash
                FROM revision_documents m JOIN document_versions v ON v.vid=m.vid
                JOIN graph_projections g ON g.gid=m.gid
                LEFT JOIN class_projections c ON c.cid=m.cid
                WHERE m.revision_id=?1 ORDER BY m.path",
            )?;
            for row in stmt.query_map([&selected.key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    graph_id_row(row,2)?,
                    class_id_row(row,3)?,
                ))
            })? {
                let (path, version, projection, class) = row?;
                selected_ensure!(
                    old.insert(path, (version, projection, class)).is_none(),
                    "incompatible_index: duplicate prior manifest document"
                );
            }
            // Equal fresh IDs under the same producer trust the validated head
            // (#225). Drift and partial-family reuse retain the R-1 witness checks.
            // These are pure per-document values, computed on workers.
            let never = AtomicBool::new(false);
            let computed = crate::parallel::map_ordered(
                &graph.files,
                crate::parallel::default_workers(),
                &never,
                |_, file| -> Result<(V8DocumentProjection, Option<String>, bool)> {
                    let grouped =
                        rows.get(file.path.as_str()).context("publication rows missing")?;
                    let ids = v8_document_projection(file, native, grouped)?;
                    let reused_version =
                        old.get(&file.path).is_some_and(|(version, _, _)| *version == ids.version_id);
                    let trusted = !drift && old.get(&file.path).is_some_and(
                        |(version, graph, class)| {
                            *version == ids.version_id
                                && *graph == ids.graph_id
                                && class.as_deref() == Some(ids.class_id.as_str())
                        },
                    );
                    let witness = if !trusted && (drift || reused_version) {
                        Some(publication_native_witness(
                            documents.get(file.path.as_str())
                                .context("new native document absent")?,
                            &native.producer,
                            coverages.get(file.path.as_str())
                                .context("new native coverage absent")?,
                            grouped,
                        )?)
                    } else {
                        None
                    };
                    Ok((ids, witness, trusted))
                },
            );
            let mut families = BTreeMap::new();
            for (file, computed) in graph.files.iter().zip(computed) {
                let (ids, measured, trusted) = computed.context("preflight projection absent")??;
                if trusted {
                    families.insert(file.path.clone(), ReusedFamilies {
                        native: true, graph: true, class: true,
                    });
                    continue;
                }
                // An equal version ID means equal content, extraction context and
                // producer version; its stored witness binds the measured facts.
                let prior_witness: Option<String> = match &measured {
                    Some(_) => db.query_row(
                        "SELECT native_witness FROM document_versions WHERE id=?1",
                        [&ids.version_id], |r| r.get(0),
                    ).optional()?,
                    None => None,
                };
                if let (Some(measured), Some(prior_witness)) = (&measured, &prior_witness)
                    && measured != prior_witness
                {
                    // The new executable FULLY measured this document. Equal
                    // document/version inputs cannot silently acquire different
                    // measured facts under Decision 0003.
                    ensure!(!drift,
                        "native_producer_version_required: measured facts changed under the same nativeProducerVersion; bump producer version before indexing");
                    return Err(ControlIntegrity(
                        "incompatible_index: reusable normalized native witness mismatch".into(),
                    )
                    .into());
                }
                if let Some((old_version, old_graph, old_class)) = old.get(&file.path) {
                    let reused = ReusedFamilies {
                        native: *old_version == ids.version_id,
                        graph: *old_graph == ids.graph_id,
                        class: old_class.as_deref() == Some(ids.class_id.as_str()),
                    };
                    selected_ensure!(
                        !reused.graph || reused.native,
                        "incompatible_index: graph reused with different native version"
                    );
                    control_ensure!(
                        !reused.class || reused.graph,
                        "incompatible_index: class reused with different graph projection"
                    );
                    if reused.native || reused.graph || reused.class {
                        control_ensure!(
                            !reused.native || prior_witness.is_some(),
                            "incompatible_index: reusable native version absent"
                        );
                        self.verify_reusable_projection(
                            db,
                            &ids,
                            reused,
                            extractions.get(&file.path),
                        )?;
                        families.insert(file.path.clone(), reused);
                    }
                }
            }
            Ok(PreflightReuse {
                pin: Some(selected.pin),
                families,
                existing_ids: BTreeMap::new(),
            })
        })
    }

    /// Read recovery extractions only after the compact plan selected the paths.
    fn load_local_class_extractions(
        &self,
        prepared: &LocalPrepared,
        paths: &BTreeSet<String>,
        cancel: &CancelFlag,
    ) -> Result<BTreeMap<String, crate::model::FileExtraction>> {
        if paths.is_empty() {
            return Ok(BTreeMap::new());
        }
        self.with_prior_publication_snapshot(|db| {
            let selected = ReadRevision::current(db)?;
            ensure!(
                selected.pin == prepared.prior_pin,
                "revision conflict: local class recovery head changed"
            );
            let mut statement = db.prepare(
                "SELECT g.class_extraction_payload FROM revision_documents m
                 JOIN graph_projections g ON g.gid=m.gid
                 WHERE m.revision_id=?1 AND m.path=?2",
            )?;
            let mut extractions = BTreeMap::new();
            for path in paths {
                check_cancel(cancel)?;
                let payload: Option<String> =
                    statement.query_row(params![selected.key, path], |row| row.get(0))?;
                let payload = payload.context(SelectedIntegrity(
                    "incompatible_index: class recovery extraction missing".into(),
                ))?;
                #[cfg(test)]
                LOCAL_CLASS_EXTRACTION_READS.with(|count| count.set(count.get() + 1));
                extractions.insert(
                    path.clone(),
                    serde_json::from_str(&payload).context(SelectedIntegrity(
                        "incompatible_index: stored class extraction decode failed".into(),
                    ))?,
                );
            }
            Ok(extractions)
        })
    }

    fn prepare_stream_class_catalog(
        spool: &crate::native_evidence::FullDocumentSpool,
        order: &[usize],
        cancel: &CancelFlag,
    ) -> Result<(crate::classes::Catalog, bool)> {
        let limits = crate::classes::Limits::default();
        // All Java/Python files were extracted and stored by the sorted native
        // owner before this publisher was entered. Replay in the same path
        // order to preserve catalog warning and class ordering.
        let mut composer =
            crate::classes::CatalogComposer::new(spool.len(), spool.stats().symbols, limits);
        let mut replay = order.to_vec();
        replay.sort_by(|a, b| {
            spool.header().revision.documents[*a]
                .key
                .path
                .as_bytes()
                .cmp(spool.header().revision.documents[*b].key.path.as_bytes())
        });
        let mut summaries = Vec::new();
        for index in replay {
            let document = &spool.header().revision.documents[index];
            if matches!(document.key.language.as_str(), "java" | "python") {
                check_cancel(cancel)?;
                let extraction = spool
                    .read_extraction(index)
                    .map_err(native_spool_read_failure)?;
                summaries.push(extraction.summary());
                if composer.is_open() {
                    composer.push(&extraction);
                }
            }
        }
        let complete = crate::classes::plan_complete_catalog(
            &summaries,
            spool.len(),
            spool.stats().symbols,
            limits,
        )
        .is_some();
        Ok((composer.finish(), complete))
    }

    fn preflight_stream_reuse(
        &self,
        spool: &crate::native_evidence::FullDocumentSpool,
        order: &[usize],
        classes: &crate::classes::Catalog,
        expected: &ExpectedPublication,
        cancel: &CancelFlag,
    ) -> Result<PreflightReuse> {
        if self.disposition() != RecoveryDisposition::Ready
            || !match expected {
                ExpectedPublication::Pin(pin) => pin.index_revision > 0,
                ExpectedPublication::Recovery(baseline) => {
                    baseline.compatible && baseline.pin().is_some_and(|p| p.index_revision > 0)
                }
            }
        {
            return Ok(PreflightReuse::default());
        }
        self.with_prior_publication_snapshot(|db| {
            let selected = ReadRevision::current(db)?;
            if !match expected {
                ExpectedPublication::Pin(pin) => *pin == selected.pin,
                ExpectedPublication::Recovery(baseline) => {
                    baseline.pin() == Some(selected.pin) && baseline.compatible
                }
            } || selected.pin.index_revision == 0
            {
                return Ok(PreflightReuse::default());
            }
            // T00 attests only versions actually reused below. Unselected
            // historical versions must not affect this fresh publication.
            let drift = selected_producer_hash(db, &selected)? != spool.header().producer.executable_hash;
            let mut old = BTreeMap::new();
            let mut stmt = db.prepare(
                "SELECT m.path,v.id,g.graph_hash,
                c.content_hash
                FROM revision_documents m JOIN document_versions v ON v.vid=m.vid
                JOIN graph_projections g ON g.gid=m.gid
                LEFT JOIN class_projections c ON c.cid=m.cid
                WHERE m.revision_id=?1 ORDER BY m.path",
            )?;
            for row in stmt.query_map([&selected.key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    graph_id_row(row,2)?,
                    class_id_row(row,3)?,
                ))
            })? {
                let (path, version, projection, class) = row?;
                selected_ensure!(
                    old.insert(path, (version, projection, class)).is_none(),
                    "incompatible_index: duplicate prior manifest document"
                );
            }
            // Keep only one document's native and graph rows in memory while
            // authenticating per-document reuse against this read snapshot.
            let mut families = BTreeMap::new();
            for &index in order {
                check_cancel(cancel)?;
                let (native, graph) = spool.read(index).map_err(native_spool_read_failure)?;
                let file = graph.files.first().context("stream graph document absent")?;
                let selected_classes = stream_document_classes(classes, &file.path);
                let rows = publication_rows(&graph, &native, &selected_classes)?;
                let grouped = rows.get(file.path.as_str()).context("publication rows missing")?;
                let ids = v8_document_projection(file, &native, grouped)?;
                if !drift && old.get(&file.path).is_some_and(|(version, graph, class)| {
                    *version == ids.version_id
                        && *graph == ids.graph_id
                        && class.as_deref() == Some(ids.class_id.as_str())
                }) {
                    families.insert(file.path.clone(), ReusedFamilies {
                        native: true, graph: true, class: true,
                    });
                    continue;
                }
                let reused_version = old.get(&file.path)
                    .is_some_and(|(version, _, _)| *version == ids.version_id);
                let measured = if drift || reused_version {
                    Some(publication_native_witness(
                        native.revision.documents.first()
                            .context("new native document absent")?,
                        &native.producer,
                        native.coverage.first().context("new native coverage absent")?,
                        grouped,
                    )?)
                } else {
                    None
                };
                // An equal version ID means equal content, extraction context and
                // producer version; its stored witness binds the measured facts.
                let prior_witness: Option<String> = match &measured {
                    Some(_) => db.query_row(
                        "SELECT native_witness FROM document_versions WHERE id=?1",
                        [&ids.version_id], |r| r.get(0),
                    ).optional()?,
                    None => None,
                };
                if let (Some(measured), Some(prior_witness)) = (&measured, &prior_witness)
                    && measured != prior_witness
                {
                    // The new executable FULLY measured this document. Equal
                    // document/version inputs cannot silently acquire different
                    // measured facts under Decision 0003.
                    ensure!(!drift,
                        "native_producer_version_required: measured facts changed under the same nativeProducerVersion; bump producer version before indexing");
                    return Err(ControlIntegrity(
                        "incompatible_index: reusable normalized native witness mismatch".into(),
                    )
                    .into());
                }
                if let Some((old_version, old_graph, old_class)) = old.get(&file.path) {
                    let reused = ReusedFamilies {
                        native: *old_version == ids.version_id,
                        graph: *old_graph == ids.graph_id,
                        class: old_class.as_deref() == Some(ids.class_id.as_str()),
                    };
                    selected_ensure!(
                        !reused.graph || reused.native,
                        "incompatible_index: graph reused with different native version"
                    );
                    control_ensure!(
                        !reused.class || reused.graph,
                        "incompatible_index: class reused with different graph projection"
                    );
                    if reused.native || reused.graph || reused.class {
                        control_ensure!(
                            !reused.native || prior_witness.is_some(),
                            "incompatible_index: reusable native version absent"
                        );
                        let extraction = if matches!(file.language.as_str(), "java" | "python") {
                            Some(spool.read_extraction(index).map_err(native_spool_read_failure)?)
                        } else {
                            None
                        };
                        self.verify_reusable_projection(
                            db,
                            &ids,
                            reused,
                            extraction.as_ref(),
                        )?;
                        families.insert(file.path.clone(), reused);
                    }
                }
            }
            Ok(PreflightReuse {
                pin: Some(selected.pin),
                families,
                existing_ids: BTreeMap::new(),
            })
        })
    }
    fn publish_inner_checked_target(
        &self,
        bundle: (
            &Graph,
            &crate::capture::Capture,
            &crate::native_evidence::Artifact,
        ),
        leader: &topology::WriterGuard,
        plan: PublicationPlan<'_>,
        cancel: &CancelFlag,
        max_graph_json_bytes: usize,
        during_tx: impl FnMut(PublishStage, &Connection) -> Result<()>,
    ) -> Result<IndexPin> {
        self.publish_inner_checked_target_with_local(
            bundle,
            leader,
            plan,
            cancel,
            (max_graph_json_bytes, None, None),
            during_tx,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_inner_checked_target_with_local(
        &self,
        bundle: (
            &Graph,
            &crate::capture::Capture,
            &crate::native_evidence::Artifact,
        ),
        leader: &topology::WriterGuard,
        plan: PublicationPlan<'_>,
        cancel: &CancelFlag,
        (max_graph_json_bytes, local, stream): (
            usize,
            Option<&LocalPrepared>,
            Option<&crate::native_evidence::FullDocumentSpool>,
        ),
        mut during_tx: impl FnMut(PublishStage, &Connection) -> Result<()>,
        semantic: Option<(
            &crate::semantic_evidence::SemanticPublication,
            &crate::semantic_evidence::CapturedComponents,
        )>,
    ) -> Result<IndexPin> {
        let PublicationPlan { expected, target } = plan;
        #[cfg(feature = "test-support")]
        if std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE").is_some() {
            crate::semantic_import::reset_publication_work(&bundle.2.revision.id, BTreeSet::new());
        }
        #[cfg(feature = "test-support")]
        let force_full_selected = std::env::var_os("TRELLIS_TEST_FORCE_FULL_REJOIN").is_some();
        #[cfg(not(feature = "test-support"))]
        let force_full_selected = false;
        let loaded_docs: Option<BTreeSet<_>> = if local.is_some() && !force_full_selected {
            Some(
                bundle
                    .2
                    .revision
                    .documents
                    .iter()
                    .filter(|doc| {
                        local.is_some_and(|prepared| prepared.changed_paths.contains(&doc.key.path))
                    })
                    .map(|doc| doc.key.clone())
                    .collect(),
            )
        } else {
            None
        };
        let selected = if semantic.is_none()
            && matches!(target, PublicationTarget::Live)
            && let ExpectedPublication::Recovery(baseline) = &expected
            && baseline.compatible
            && let Some(pin) = baseline.pin().filter(|pin| pin.index_revision > 0)
            && self.disposition() != RecoveryDisposition::Rebuild
        {
            if self.selected_links_for_reconciliation(pin)? {
                self.selected_native_inputs_at(pin, loaded_docs.as_ref())?
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };
        if let PublicationTarget::Live = target {
            self.ensure_not_recreate_pending()?;
        }
        let (graph, capture, native) = bundle;
        let source_files = if stream.is_some() {
            capture.files.as_slice()
        } else {
            graph.files.as_slice()
        };
        let stream_order = if stream.is_some() {
            let by_path: BTreeMap<_, _> = native
                .revision
                .documents
                .iter()
                .enumerate()
                .map(|(index, doc)| (doc.key.path.as_str(), index))
                .collect();
            ensure!(
                by_path.len() == native.revision.documents.len(),
                "native_evidence_required: duplicate revision document path"
            );
            source_files
                .iter()
                .map(|file| {
                    by_path
                        .get(file.path.as_str())
                        .copied()
                        .context("native document missing captured graph path")
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        Self::enforce_selected_source_admission(graph, source_files, max_graph_json_bytes)?;
        ensure!(
            graph.schema_version == SCHEMA_VERSION,
            "unsupported graph schema"
        );
        check_cancel(cancel)?;
        let (stats, classes, extractions, class_complete) = if let Some(spool) = stream {
            ensure!(
                local.is_none(),
                "local and streamed publication cannot be combined"
            );
            let (classes, complete) =
                Self::prepare_stream_class_catalog(spool, &stream_order, cancel)?;
            (spool.stats().clone(), classes, BTreeMap::new(), complete)
        } else {
            let stats = if let Some(prepared) = local {
                ensure!(
                    prepared.prior_pin == expected.pin().context("local expected pin missing")?,
                    "revision conflict: local prepared pin changed"
                );
                graph.stats.clone()
            } else {
                validate_graph(graph, cancel)?
            };
            // Parse cached source before taking the writer lock. Projection and graph
            // still publish in one transaction with the same CAS/cancellation guard.
            let limits = crate::classes::Limits::default();
            let class_paths: BTreeSet<_> = graph
                .files
                .iter()
                .filter(|f| matches!(f.language.as_str(), "java" | "python"))
                .map(|f| f.path.as_str())
                .collect();
            // Class extraction only consumes class/method/function symbols. Supply
            // each file's symbols once instead of rescanning the full graph per file.
            let mut class_symbols: BTreeMap<&str, Vec<Symbol>> = BTreeMap::new();
            for node in &graph.nodes {
                if class_paths.contains(node.path.as_str())
                    && matches!(
                        node.kind,
                        SymbolKind::Class | SymbolKind::Method | SymbolKind::Function
                    )
                {
                    class_symbols
                        .entry(node.path.as_str())
                        .or_default()
                        .push(node.clone());
                }
            }
            let mut extractions: BTreeMap<String, crate::model::FileExtraction> = graph
                .files
                .iter()
                .filter(|file| {
                    class_paths.contains(file.path.as_str())
                        && local.is_none_or(|prepared| prepared.changed_paths.contains(&file.path))
                })
                .map(|file| {
                    Ok((
                        file.path.clone(),
                        crate::model::FileExtraction::extract_file(
                            file,
                            class_symbols
                                .get(file.path.as_str())
                                .map(Vec::as_slice)
                                .unwrap_or(&[]),
                            cancel,
                            limits,
                        )?,
                    ))
                })
                .collect::<Result<_>>()?;
            let mut summaries: BTreeMap<_, _> = extractions
                .iter()
                .map(|(path, extraction)| (path.clone(), extraction.summary()))
                .collect();
            if let Some(prepared) = local {
                for path in &class_paths {
                    if !prepared.changed_paths.contains(*path) {
                        let summary =
                            prepared
                                .prior_summaries
                                .get(*path)
                                .context(SelectedIntegrity(
                                    "incompatible_index: local class summary inventory incomplete"
                                        .into(),
                                ))?;
                        summaries.insert((*path).to_owned(), summary.clone());
                    }
                }
            }
            selected_ensure!(
                summaries.len() == class_paths.len(),
                "incompatible_index: local class summary inventory incomplete"
            );
            let metadata = crate::classes::plan_complete_catalog(
                summaries.values(),
                graph.files.len(),
                graph.stats.symbols,
                limits,
            );
            let complete = metadata.is_some();
            if let Some(prepared) = local {
                let recovery_paths = class_paths
                    .iter()
                    .filter(|path| {
                        !prepared.changed_paths.contains(**path)
                            && (!complete
                                || !prepared
                                    .prior_class_complete
                                    .get(**path)
                                    .copied()
                                    .unwrap_or(false))
                    })
                    .map(|path| (*path).to_owned())
                    .collect();
                extractions.extend(self.load_local_class_extractions(
                    prepared,
                    &recovery_paths,
                    cancel,
                )?);
            }
            let classes = if local.is_some()
                && let Some(metadata) = metadata
            {
                // Sparse payload: complete unchanged selections keep their immutable CID.
                // Only changed and recovered files contribute class rows here.
                let mut classes = crate::classes::Catalog {
                    classes: Vec::new(),
                    relations: Vec::new(),
                    warnings: metadata.warnings,
                    truncated: metadata.truncated,
                };
                for extraction in extractions.values() {
                    check_cancel(cancel)?;
                    let file = crate::classes::complete_file_catalog(extraction);
                    classes.classes.extend(file.classes);
                    classes.relations.extend(file.relations);
                }
                classes
            } else {
                Self::compose_selected_class_catalog(graph, &extractions, limits)?
            };
            (stats, classes, extractions, complete)
        };
        // The public read_status guard has this exact singleton JSON ceiling.
        // Refuse over-limit catalog warnings before paired CAS.
        ensure!(
            classes.relations.iter().all(|r| r.target.is_none()
                && r.candidate_ids.is_empty()
                && r.match_kind == "unmatched"),
            "unsafe_index: lexical class relationship"
        );
        ensure!(
            json(&classes.warnings)?.len() <= 256 * 1024,
            "incompatible_index: class catalog byte budget exceeded before publication"
        );
        let rows = if stream.is_some() {
            BTreeMap::new()
        } else {
            publication_rows(graph, native, &classes)?
        };
        check_cancel(cancel)?;
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        let publication = (|| {
            let mut db = match target {
                PublicationTarget::Live => PublicationConnection::Live(self.cache_write()?),
                PublicationTarget::Stage(stage) => {
                    leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
                    stage.verify_path()?;
                    let db = open_index(&stage.path, true)?;
                    stage.verify_path()?;
                    PublicationConnection::Stage(db)
                }
            };
            // Keep this exact SQLite connection open across preflight. data_version
            // values from different connections cannot fence a concurrent SQL rewrite.
            let admitted_version: i64 =
                storage_result(db.pragma_query_value(None, "data_version", |r| r.get(0)))?;
            #[cfg(test)]
            if let Some(spool) = stream {
                let hook = self
                    .test_stream_before_preflight_hook
                    .lock()
                    .unwrap()
                    .take();
                if let Some(hook) = hook {
                    hook(spool);
                }
            }
            let reuse = if let Some(prepared) = local {
                ensure!(
                    matches!(target, PublicationTarget::Live),
                    "local evidence cannot use a private stage"
                );
                self.preflight_local_reuse(graph, prepared, &rows, class_complete)?
            } else if let Some(spool) = stream {
                self.preflight_stream_reuse(spool, &stream_order, &classes, &expected, cancel)?
            } else if matches!(target, PublicationTarget::Live) {
                // A stored hash that differs from the fresh one is typed index
                // corruption: the next publication rebaselines from fresh facts.
                self.preflight_reuse(graph, native, &extractions, &expected, &rows)?
            } else {
                PreflightReuse::default()
            };
            let after_preflight: i64 =
                storage_result(db.pragma_query_value(None, "data_version", |r| r.get(0)))?;
            ensure!(
                after_preflight == admitted_version,
                "incompatible_index: cache changed during reuse authentication"
            );
            // A publication is one transaction; with SQLite's default ~2 MiB cache its
            // dirty pages spill to the file mid-transaction (#209). This connection is
            // opened per publication, so the larger cache lasts only as long as it does.
            let cache_kib = self.publication_cache_kib.load(Ordering::Acquire);
            storage_result(db.pragma_update(None, "cache_size", -cache_kib))?;
            during_tx(PublishStage::BeforeTransaction, &db)?;
            // Only the live index has readers; a private stage has none. Declared
            // before the transaction so it drops after it: on an error or unwind
            // the mark stays set until the rollback has released the lock.
            let mut readers_blocked = matches!(target, PublicationTarget::Live)
                .then(|| ReadersBlockedWatch::new(&self.publication_gate, source_files.len()));
            let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
            let current = match target {
                PublicationTarget::Live => self.recovery_baseline_preflight(&tx)?,
                PublicationTarget::Stage(_) => self.recovery_baseline(&tx)?,
            };
            let locked_version: i64 =
                storage_result(tx.pragma_query_value(None, "data_version", |r| r.get(0)))?;
            ensure!(
                locked_version == admitted_version,
                "incompatible_index: cache changed after admission"
            );
            match &expected {
                ExpectedPublication::Pin(expected_pin) => ensure!(
                    current.pin == Some(*expected_pin),
                    "revision conflict: expected {expected_pin:?}, found {:?}",
                    current.pin
                ),
                ExpectedPublication::Recovery(expected_baseline) => ensure!(
                    current.witness == expected_baseline.witness
                        && current.pin == expected_baseline.pin,
                    "revision conflict: private recovery baseline changed"
                ),
            }
            ensure!(
                reuse.pin.is_none() || reuse.pin == current.pin,
                "revision conflict: authenticated reuse snapshot changed"
            );
            let compatible = current.compatible;
            let old_pin = current.pin;
            let schema: u32 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
            // The compatible old snapshot was decoded under preflight's read snapshot;
            // the data_version and expected-pin CAS above bind it to this transaction.
            let decoded = compatible;
            if let PublicationTarget::Live = target {
                self.ensure_not_recreate_pending()?;
            }
            ensure!(
                schema == DATABASE_SCHEMA_VERSION,
                "incompatible_index: unsupported schema cannot be rebaselined"
            );
            let rebaseline = !compatible
                || !decoded
                || (matches!(target, PublicationTarget::Live)
                    && self.disposition() == RecoveryDisposition::Rebuild);
            let prior_scan = (!rebaseline)
                .then(|| compare_capture_snapshot(&tx, capture))
                .transpose()?;
            if let Some(comparison) = prior_scan {
                ensure!(
                    comparison.examined > 0,
                    "incompatible_index: empty prior scan comparison"
                );
                let _changed = comparison.changed();
            }
            ensure!(
                source_files.len() == native.revision.documents.len(),
                "graph/native document cardinality mismatch"
            );
            let mut documents_by_key: HashMap<(&str, &str), Vec<_>> = HashMap::new();
            for doc in &native.revision.documents {
                documents_by_key
                    .entry((doc.key.language.as_str(), doc.key.path.as_str()))
                    .or_default()
                    .push(doc);
            }
            let mut captured_by_key: HashMap<(&str, &str), Vec<_>> = HashMap::new();
            for source in &capture.files {
                captured_by_key
                    .entry((source.language.as_str(), source.path.as_str()))
                    .or_default()
                    .push(source);
            }
            for file in source_files {
                let key = (file.language.as_str(), file.path.as_str());
                ensure!(
                    documents_by_key
                        .get(&key)
                        .is_some_and(|docs| docs.iter().any(|doc| {
                            doc.content_hash == file.hash && doc.byte_length == file.text.len()
                        }))
                        && captured_by_key
                            .get(&key)
                            .is_some_and(|sources| sources.contains(&file)),
                    "graph/native captured source mismatch"
                );
            }
            let revision = IndexPin {
                index_generation: if rebaseline {
                    uuid::Uuid::new_v4()
                } else {
                    old_pin
                        .context("index_not_ready: prior index pin is not decodable")?
                        .index_generation
                },
                index_revision: if rebaseline {
                    1
                } else {
                    old_pin
                        .context("index_not_ready: prior index pin is not decodable")?
                        .index_revision
                        .checked_add(1)
                        .filter(|n| *n <= 9_007_199_254_740_991)
                        .context("revision overflow")?
                },
            };
            if !rebaseline {
                validate_supersessions(&tx)?;
            }
            if rebaseline {
                tx.execute_batch(
                    "DELETE FROM revision_producer_bindings; DELETE FROM native_binding_epoch;",
                )?;
            }
            if rebaseline {
                // An incompatible/corrupt generation is explicitly replaced, never served.
                // Only compatible same-generation publication appends history.
                tx.execute_batch("DELETE FROM semantic_reference_roles; DELETE FROM semantic_reference_candidates; DELETE FROM semantic_symbol_targets; DELETE FROM semantic_declaration_binding_symbols; DELETE FROM semantic_declaration_join_candidates; DELETE FROM semantic_call_candidates; DELETE FROM semantic_call_possible_dispatch; DELETE FROM semantic_call_join_candidates; DELETE FROM semantic_type_relationships; DELETE FROM semantic_references; DELETE FROM semantic_symbols; DELETE FROM semantic_declaration_bindings; DELETE FROM semantic_call_bindings; DELETE FROM semantic_provenance; DELETE FROM semantic_revision_raw_selections; DELETE FROM semantic_raw_proofs; DELETE FROM semantic_raw_captures; DELETE FROM semantic_raw_exclusions; DELETE FROM semantic_raw_batches; DELETE FROM semantic_import_reports; DELETE FROM semantic_basis_lookup_dependencies; DELETE FROM semantic_basis; DELETE FROM semantic_coverage_roles; DELETE FROM semantic_coverage; DELETE FROM semantic_requested_producer_languages; DELETE FROM semantic_requested_producers; DELETE FROM semantic_revision_producers; DELETE FROM semantic_producer_languages; DELETE FROM semantic_producers; DELETE FROM revision_documents; DELETE FROM revision_capture_inputs;
                DELETE FROM native_version_references; DELETE FROM native_version_capability_gaps;
                DELETE FROM native_version_capability_families;
                DELETE FROM native_version_call_regions; DELETE FROM native_version_calls;
                DELETE FROM native_version_control_regions; DELETE FROM native_version_own_signature_types;
                DELETE FROM native_version_ancestor_signature_types; DELETE FROM native_version_parameters;
                DELETE FROM native_version_header_items; DELETE FROM native_version_headers;
                DELETE FROM head_search_fts; DELETE FROM native_version_search_cards;
                DELETE FROM native_version_declaration_ancestors; DELETE FROM native_version_declarations;
                DELETE FROM native_version_coverage_roles; DELETE FROM class_relations; DELETE FROM classes;
                DELETE FROM class_projections; DELETE FROM graph_display_labels;
                DELETE FROM graph_nodes; DELETE FROM graph_projections; DELETE FROM document_versions;
                DELETE FROM native_release_candidate_classes; DELETE FROM native_release_candidate_graphs;
                DELETE FROM native_release_candidate_versions; DELETE FROM native_revision_release_debt;
                DELETE FROM native_revision_supersessions; DELETE FROM native_revisions; DELETE FROM native_source_set_dependencies;
                DELETE FROM native_source_set_languages; DELETE FROM native_source_sets;
                DELETE FROM native_producer_inputs; DELETE FROM native_producer_languages;
                DELETE FROM native_producers;")?;
            }
            // A fresh schema has the binding tables but no epoch until its first
            // publication. Rebaseline removes the prior epoch and creates a new one.
            let epoch_exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM native_binding_epoch WHERE singleton=1)",
                [],
                |row| row.get(0),
            )?;
            if !epoch_exists {
                control_ensure!(
                    rebaseline || old_pin.is_some_and(|pin| pin.index_revision == 0),
                    "incompatible_index: selected producer binding epoch invalid"
                );
                tx.execute(
                    "INSERT INTO native_binding_epoch VALUES(1,?1,?2)",
                    params![
                        revision.index_generation.to_string(),
                        revision.index_revision as i64
                    ],
                )?;
            }
            // The active manifest is full-rewrite, but old same-generation manifests
            // and their immutable version/projection trees remain for retained pins.
            // ImmutableAppend issues a few statement shapes per table for every row.
            tx.set_prepared_statement_cache_capacity(512);
            let mut immutable = ImmutableAppend::default();
            let projections = if let Some(spool) = stream {
                let mut seen_paths = BTreeSet::new();
                // Persist the full captured revision header exactly once, including
                // the empty-workspace case. Per-document Artifacts keep only their
                // own document rather than cloning the full revision header.
                let empty_graph = Graph::default();
                let header_rows = BTreeMap::new();
                write_native(
                    &tx,
                    (native, capture, &empty_graph, &classes, &extractions),
                    revision,
                    &stats,
                    cancel,
                    &mut immutable,
                    &reuse,
                    &header_rows,
                    &graph.diagnostics,
                    class_complete,
                    NativeWrite::Header { ordinal_start: 0 },
                )?;
                // Decode a bounded batch, hash its projections and witnesses on
                // workers, then write it in order on this connection.
                let batch_len = crate::parallel::default_workers() * 16;
                for (batch_index, batch) in stream_order.chunks(batch_len).enumerate() {
                    let mut decoded = Vec::with_capacity(batch.len());
                    for (offset, &index) in batch.iter().enumerate() {
                        check_cancel(cancel)?;
                        let ordinal = batch_index * batch_len + offset;
                        let (artifact, document_graph) =
                            spool.read(index).map_err(native_spool_read_failure)?;
                        ensure!(
                            artifact.producer == native.producer
                                && artifact.source_set == native.source_set
                                && artifact.revision.id == native.revision.id
                                && artifact.revision.source_set_id == native.revision.source_set_id
                                && artifact.revision.toolchain_hash
                                    == native.revision.toolchain_hash
                                && artifact.revision.config_hash == native.revision.config_hash
                                && artifact.revision.dependency_hash
                                    == native.revision.dependency_hash
                                && artifact.revision.documents.as_slice()
                                    == std::slice::from_ref(&native.revision.documents[index]),
                            "native_evidence_required: streamed revision header differs"
                        );
                        let file = document_graph
                            .files
                            .first()
                            .context("stream graph document absent")?;
                        ensure!(
                            document_graph.files.len() == 1
                                && source_files.get(ordinal) == Some(file),
                            "native_evidence_required: streamed graph document differs from capture"
                        );
                        Self::enforce_selected_source_admission(
                            &document_graph,
                            &document_graph.files,
                            max_graph_json_bytes,
                        )?;
                        let selected_classes = stream_document_classes(&classes, &file.path);
                        let extractions = if matches!(file.language.as_str(), "java" | "python") {
                            BTreeMap::from([(
                                file.path.clone(),
                                spool
                                    .read_extraction(index)
                                    .map_err(native_spool_read_failure)?,
                            )])
                        } else {
                            BTreeMap::new()
                        };
                        decoded.push((
                            ordinal,
                            artifact,
                            document_graph,
                            selected_classes,
                            extractions,
                        ));
                    }
                    let rows = decoded
                        .iter()
                        .map(|(_, artifact, document_graph, selected_classes, _)| {
                            publication_rows(document_graph, artifact, selected_classes)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    let mut precomputed = crate::parallel::map_ordered(
                        &decoded,
                        crate::parallel::default_workers(),
                        cancel,
                        |offset, (_, artifact, document_graph, _, _)| {
                            let file = &document_graph.files[0];
                            precompute_document(
                                file,
                                artifact,
                                rows[offset].get(file.path.as_str()),
                                artifact.revision.documents.first(),
                                artifact.coverage.first(),
                                &reuse,
                            )
                        },
                    )
                    .into_iter();
                    for ((ordinal, artifact, document_graph, _, extractions), rows) in
                        decoded.iter().zip(&rows)
                    {
                        check_cancel(cancel)?;
                        let file = &document_graph.files[0];
                        let written = write_native(
                            &tx,
                            (artifact, capture, document_graph, &classes, extractions),
                            revision,
                            &stats,
                            cancel,
                            &mut immutable,
                            &reuse,
                            rows,
                            &graph.diagnostics,
                            class_complete,
                            NativeWrite::Document {
                                ordinal: *ordinal,
                                captured_file: source_files
                                    .get(*ordinal)
                                    .context("streamed captured document ordinal missing")?,
                                precomputed: precomputed
                                    .next()
                                    .flatten()
                                    .context("indexing cancelled")?,
                            },
                        )?;
                        let ids = written
                            .get(&file.path)
                            .context("missing streamed document projection")?;
                        during_tx(PublishStage::AfterFile, &tx)?;
                        if let Some(watch) = readers_blocked.as_mut() {
                            watch.check(&tx);
                        }
                        let grouped = rows
                            .get(file.path.as_str())
                            .context("streamed publication rows missing")?;
                        write_graph_document(
                            &tx,
                            grouped,
                            ids,
                            reuse.for_path(&file.path),
                            &mut immutable,
                        )?;
                        ensure!(
                            seen_paths.insert(file.path.clone()),
                            "duplicate streamed document path"
                        );
                    }
                }
                BTreeMap::new()
            } else {
                write_native(
                    &tx,
                    (native, capture, graph, &classes, &extractions),
                    revision,
                    &stats,
                    cancel,
                    &mut immutable,
                    &reuse,
                    &rows,
                    &graph.diagnostics,
                    class_complete,
                    NativeWrite::Header { ordinal_start: 0 },
                )?
            };
            let revision_key = format!(
                "pin:v1:{}:{}",
                revision.index_generation, revision.index_revision
            );
            let (executing_hash, binding_sha) = revision_producer_binding(&tx, &revision_key)?;
            ensure!(
                executing_hash == native.producer.executable_hash,
                "incompatible_index: published producer differs from captured executable"
            );
            tx.execute(
                "INSERT INTO revision_producer_bindings VALUES(?1,?2,?3)",
                params![revision_key, executing_hash, binding_sha],
            )?;
            if stream.is_none() {
                for f in source_files {
                    check_cancel(cancel)?;
                    during_tx(PublishStage::AfterFile, &tx)?;
                    if let Some(watch) = readers_blocked.as_mut() {
                        watch.check(&tx);
                    }
                    let ids = projections
                        .get(&f.path)
                        .context("missing graph projection")?;
                    let grouped = rows
                        .get(f.path.as_str())
                        .context("publication rows missing")?;
                    write_graph_document(
                        &tx,
                        grouped,
                        ids,
                        reuse.for_path(&f.path),
                        &mut immutable,
                    )?;
                }
            }
            immutable.finish(&tx)?;
            // The spool header carries the complete revision but no per-document facts.
            // Reconstruct the measured inventory from that same private spool for the
            // semantic validator; never validate against the header's empty fact lists.
            let measured_stream = if (semantic.is_some() || !selected.is_empty())
                && let Some(spool) = stream
            {
                let mut measured = native.clone();
                for index in 0..spool.len() {
                    check_cancel(cancel)?;
                    let (document, _) = spool.read(index).map_err(native_spool_read_failure)?;
                    measured.coverage.extend(document.coverage);
                    measured.provenance.extend(document.provenance);
                    measured.declarations.extend(document.declarations);
                    measured.calls.extend(document.calls);
                    measured.control_regions.extend(document.control_regions);
                    measured.references.extend(document.references);
                    measured
                        .capability_reports
                        .extend(document.capability_reports);
                }
                Some(measured)
            } else {
                None
            };
            if let Some((semantic, captured)) = semantic {
                ensure!(
                    semantic
                        .provenance
                        .iter()
                        .all(|proof| proof.revision_id == native.revision.id),
                    "captured proof revision differs from candidate native revision"
                );
                crate::semantic_evidence::validate_evidence(
                    semantic,
                    measured_stream.as_ref().unwrap_or(native),
                    captured,
                )?;
            }
            write_semantic(
                &tx,
                native,
                old_pin.filter(|pin| !rebaseline && pin.index_revision > 0),
                revision,
                semantic,
                &selected,
            )?;
            if !selected.is_empty() {
                let candidate = measured_stream.as_ref().unwrap_or(native);
                let sources = source_files
                    .iter()
                    .map(|file| {
                        Ok((
                            crate::native_evidence::DocumentKey {
                                source_set_id: candidate.revision.source_set_id.clone(),
                                language: file.language.clone(),
                                path: file.path.clone(),
                            },
                            file.text.as_bytes().to_vec(),
                        ))
                    })
                    .collect::<Result<BTreeMap<_, _>>>()?;
                #[cfg(feature = "test-support")]
                self.publication_before_semantic_hook.run();
                check_cancel(cancel)?;
                selected_native_rejoin(
                    self,
                    &tx,
                    &selected,
                    candidate,
                    &sources,
                    revision,
                    cancel,
                    local.map(|_| &reuse),
                    old_pin.filter(|_| !rebaseline),
                )
                .map_err(rejoin_storage_error)?;
                if let Some(prior) = old_pin.filter(|pin| !rebaseline && pin.index_revision > 0) {
                    fill_semantic_coverage(
                        &tx,
                        &semantic_pin_key(prior),
                        &semantic_pin_key(revision),
                    )?;
                }
            }
            search::publish_head(&tx, &revision_key)?;
            let publication_time = self.retention_time()?;
            ensure!(
                publication_time.as_secs() <= 9_007_199_254_740_991,
                "invalid_supersession_time"
            );
            let timestamp = publication_time.as_millis().to_string();
            if !rebaseline && let Some(prior) = old_pin.filter(|pin| pin.index_revision > 0) {
                let old_key = format!("pin:v1:{}:{}", prior.index_generation, prior.index_revision);
                stamp_predecessor(&tx, &old_key, publication_time.as_secs() as i64)?;
            }
            if rebaseline {
                tx.execute(
                "INSERT INTO index_metadata(singleton,schema_version,extractor_version,root_spelling,
                    root_device,root_inode,index_generation,index_revision,last_opened_at,
                    indexed_at,stats,diagnostics,reconcile_options)
                 SELECT 1,?9,?1,?2,?3,?4,?5,0,?6,'',?7,?8,NULL
                 WHERE NOT EXISTS(SELECT 1 FROM index_metadata)",
                params![
                    EXTRACTOR_VERSION,
                    self.workspace_root,
                    self.identity.device.to_string(),
                    self.identity.inode.to_string(),
                    revision.index_generation.to_string(),
                    publication_time.as_secs() as i64,
                    json(&IndexStats::default())?,
                    json(&Vec::<Diagnostic>::new())?,
                    DATABASE_SCHEMA_VERSION
                ],
            )?;
                tx.execute("UPDATE index_metadata SET root_spelling=?1,root_device=?2,root_inode=?3 WHERE singleton=1",
                params![self.workspace_root, self.identity.device.to_string(), self.identity.inode.to_string()])?;
            }
            tx.execute("UPDATE index_metadata SET schema_version=?8,extractor_version=?1,index_generation=?2,index_revision=?3,indexed_at=?4,stats=?5,diagnostics=?6,reconcile_options=?7 WHERE singleton=1",
            params![EXTRACTOR_VERSION, revision.index_generation.to_string(), revision.index_revision as i64,timestamp,json(&stats)?,json(&graph.diagnostics)?,json(capture.reconcile_options())?,DATABASE_SCHEMA_VERSION])?;
            immutable.counters.record(
                "manifest",
                &[
                    &EXTRACTOR_VERSION,
                    &revision.index_generation.to_string(),
                    &(revision.index_revision as i64),
                    &timestamp,
                    &json(&stats)?,
                    &json(&graph.diagnostics)?,
                    &json(capture.reconcile_options())?,
                ],
            )?;
            if rebaseline {
                tx.pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION)?;
            }
            validate_paired_metadata(&tx, &self.identity.record_id, PairedManifestScope::Changing)?;
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM revision_documents WHERE revision_id=?1",
                [format!(
                    "pin:v1:{}:{}",
                    revision.index_generation, revision.index_revision
                )],
                |r| r.get(0),
            )?;
            ensure!(
                count == source_files.len() as i64,
                "incompatible_index: incomplete new revision manifest"
            );
            check_cancel(cancel)?;
            capture.verify(cancel)?;
            leader.verify()?;
            self.identity.verify()?;
            if let PublicationTarget::Stage(stage) = target {
                stage.verify_path()?;
                leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
            }
            if let Some(watch) = readers_blocked.as_mut() {
                watch.check(&tx);
            }
            self.publication_before_commit_hook.run();
            during_tx(PublishStage::BeforeCommit, &tx)?;
            check_cancel(cancel)?;
            capture.verify(cancel)?;
            leader.verify()?;
            self.identity.verify()?;
            if let PublicationTarget::Stage(stage) = target {
                stage.verify_path()?;
                leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
            }
            #[cfg(test)]
            if matches!(target, PublicationTarget::Live)
                && self
                    .test_publish_commit_busy_once
                    .swap(false, Ordering::AcqRel)
            {
                // Deterministic pre-commit rollback-journal contention: the live
                // transaction drops without publishing any part of this revision.
                anyhow::bail!("storage_busy: SQLite lock contention");
            }
            #[cfg(test)]
            if matches!(target, PublicationTarget::Live)
                && self
                    .test_publish_commit_typed_busy_once
                    .swap(false, Ordering::AcqRel)
            {
                // Same rollback point, but a direct typed SQLite contention.
                return Err(SqliteContention(()).into());
            }
            storage_result(tx.commit())?;
            self.published_admission.invalidate();
            // COMMIT released the lock; later failures are not a held rebuild.
            drop(readers_blocked);
            #[cfg(test)]
            if matches!(target, PublicationTarget::Live)
                && self
                    .test_publish_post_commit_busy_once
                    .swap(false, Ordering::AcqRel)
            {
                // Simulate an ambiguous post-COMMIT failure: the new pin is on
                // disk, but the caller has no successful publication result.
                anyhow::bail!("storage_busy: SQLite lock contention");
            }
            *self.writer_counters.lock().unwrap() = Some(immutable.counters);
            match target {
                PublicationTarget::Live => {
                    self.recovery_disposition
                        .store(RecoveryDisposition::Ready as u8, Ordering::Release);
                    self.recovery_required.store(false, Ordering::Release);
                }
                PublicationTarget::Stage(stage) => {
                    stage.verify_path()?;
                    leader.verify_exclusive_use(&self.roots.index_use_lock(&self.identity))?;
                }
            }
            Ok(revision)
        })();
        match target {
            PublicationTarget::Live => {
                publication.map_err(|error| self.report_index_failure(error))
            }
            PublicationTarget::Stage(_) => publication,
        }
    }

    /// A selected manifest is bound to the same admitted SQLite snapshot as its
    /// consumers. Only typed UUID/integer pin components enter these SQL literals;
    /// the caller never supplies SQL or a raw revision key.
    fn read_revision(&self, db: &Connection, expected: Option<IndexPin>) -> Result<ReadRevision> {
        self.read_revision_for(db, expected, false)
    }
    fn read_revision_for(
        &self,
        db: &Connection,
        expected: Option<IndexPin>,
        pre_startup: bool,
    ) -> Result<ReadRevision> {
        // A response's own Store already validated its head at admission and
        // every read of the response shares that snapshot.
        let head = match &self.admitted {
            Some((admitted, _)) if pre_startup => *admitted,
            _ => self.read_status_for(db, pre_startup)?.revision,
        };
        let pin = expected.unwrap_or(head);
        ensure!(
            pin.index_generation == head.index_generation
                && pin.index_revision > 0
                && pin.index_revision <= head.index_revision,
            "revision conflict: foreign or missing native pin"
        );
        let key = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
        let state: Option<bool> = db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_capture_inputs i
                WHERE i.revision_id=r.id AND i.input_key IN ('__released:v1','__pending_release:v1'))
             FROM native_revisions r WHERE r.id=?1 AND r.published_index_revision=?2",
                params![key, pin.index_revision as i64],
                |r| r.get(0),
            )
            .optional()?;
        match state {
            Some(true) => return Err(PinExpired.into()),
            None if pin == head => {
                return Err(SelectedIntegrity("selected current revision missing".into()).into());
            }
            None => anyhow::bail!("revision conflict: missing native pin"),
            Some(false) => {}
        }
        let mut selected = ReadRevision::new(pin, key);
        // Within one response's snapshot, the admitted pin's successful
        // producer binding is shared; every check above ran again.
        if let Some((admitted, headers)) = &self.admitted
            && *admitted == pin
        {
            selected.headers = headers.clone();
        }
        selected.producer_hash(db)?;
        Ok(selected)
    }

    fn native_at<T>(
        &self,
        pin: IndexPin,
        read: impl FnOnce(&Connection, &ReadRevision) -> Result<T>,
    ) -> Result<T> {
        self.with_evidence(|db| {
            let selected = self.read_revision(db, Some(pin))?;
            read(db, &selected)
        })
    }

    /// Runs `read` over one retained native revision in a single read transaction.
    pub fn with_native_reader<T>(
        &self,
        pin: IndexPin,
        read: impl FnOnce(&NativeReader<'_>) -> Result<T>,
    ) -> Result<T> {
        self.native_at(pin, |db, selected| {
            // The response connection is per request: bound its page cache for batch staging.
            let kib = crate::semantic_batch::limits::NATIVE_READER_CACHE_KIB;
            storage_result(db.pragma_update(None, "cache_size", -i64::try_from(kib)?))?;
            read(&NativeReader {
                store: self,
                db,
                selected,
            })
        })
    }

    fn native_at_reconcile<T>(
        &self,
        pin: IndexPin,
        read: impl FnOnce(&Connection, &ReadRevision) -> Result<T>,
    ) -> Result<T> {
        (|| {
            let mut db = self.cache()?;
            let tx = storage_result(db.transaction())?;
            begin_read_snapshot(
                &tx,
                &self.roots.index_db(&self.identity),
                Duration::from_secs(5),
            )?;
            let selected = self.read_revision_for(&tx, Some(pin), true)?;
            read(&tx, &selected)
        })()
        .map_err(|error| self.report_index_failure(error))
    }

    fn selected_links_for_reconciliation(&self, pin: IndexPin) -> Result<bool> {
        (|| {
            let db = self.cache()?;
            let key = semantic_pin_key(pin);
            Ok(db.query_row(
                "SELECT EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1)",
                [&key],
                |row| row.get(0),
            )?)
        })()
        .map_err(|error| self.report_index_failure(error))
    }

    /// Resolve one deduplicated captured proof by its authenticated capture tuple.
    pub fn semantic_raw_proof_at(
        &self,
        producer_id: &str,
        artifact_hash: &str,
        source_set_id: &str,
        captured_revision: &str,
        proof_id: &str,
    ) -> Result<Option<StoredRawProof>> {
        self.with_evidence(|db| {
            let row: Option<(String,String,String,String,Vec<u8>)> = db.query_row("SELECT p.record_kind,p.fact_digest,p.language,p.path,p.canonical_bytes FROM semantic_raw_captures c JOIN semantic_raw_proofs p ON p.capture_id=c.capture_id WHERE c.producer_id=?1 AND c.artifact_hash=?2 AND c.source_set_id=?3 AND c.captured_revision=?4 AND p.proof_id=?5",params![producer_id,artifact_hash,source_set_id,captured_revision,proof_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
            row.map(|(record_kind,fact_digest,language,path,canonical_bytes)| {
                let envelope: serde_json::Value = serde_json::from_slice(&canonical_bytes)
                    .context(SelectedIntegrity("incompatible_index: corrupt raw proof envelope".into()))?;
                selected_ensure!(envelope["recordKind"] == record_kind && envelope["relativePath"] == path
                    && crate::native_ids::digest(b"trellis.semantic-fact.v1\0", &canonical_bytes) == fact_digest
                    && crate::native_ids::digest(b"trellis.capture-proof.v1\0", &crate::native_ids::canonical(&serde_json::json!({"producerId":producer_id,"artifactHash":artifact_hash,"sourceSetId":source_set_id,"sourceRevision":captured_revision,"language":language,"relativePath":path,"factDigest":fact_digest}))) == proof_id,"incompatible_index: corrupt raw proof inventory");
                Ok(StoredRawProof {record_kind,fact_digest,language,path,canonical_bytes,artifact_hash: artifact_hash.to_owned()})
            }).transpose()
        })
    }

    #[cfg(feature = "test-support")]
    pub fn with_index_sql_for_tests<T>(
        &self,
        write: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        let db = self.cache_write()?;
        write(&db)
    }

    /// Freeze one pin's selected captures and requested tuple attribution in producer order.
    pub fn selected_raw_inputs_at(&self, pin: IndexPin) -> Result<Vec<SelectedRawBatch>> {
        self.selected_raw_input_results_at(pin)?
            .into_iter()
            .map(|(_, _, input)| input)
            .collect()
    }

    pub(crate) fn selected_raw_input_results_at(
        &self,
        pin: IndexPin,
    ) -> Result<Vec<SelectedRawBatchResult>> {
        self.selected_raw_input_results_at_with(pin, false, false)
    }

    fn selected_raw_input_results_at_with(
        &self,
        pin: IndexPin,
        reconciling: bool,
        metadata_only: bool,
    ) -> Result<Vec<SelectedRawBatchResult>> {
        let read = |db: &Connection, selected: &ReadRevision| {
            self.selected_raw_input_results_from_db(db, selected, pin, metadata_only)
        };
        if reconciling {
            self.native_at_reconcile(pin, read)
        } else {
            self.native_at(pin, read)
        }
    }

    fn selected_raw_input_results_from_db(
        &self,
        db: &Connection,
        selected: &ReadRevision,
        pin: IndexPin,
        metadata_only: bool,
    ) -> Result<Vec<SelectedRawBatchResult>> {
        let mut inputs = Vec::new();
        let links: Vec<(String, i64, String)> = db.prepare(
            "SELECT producer_id,batch_id,captured_pin FROM semantic_revision_raw_selections WHERE revision_id=?1 ORDER BY producer_id"
        )?.query_map([&selected.key], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (producer_id, batch_id, captured_key) in links {
            let descriptor: Option<(String,String,String,String)> = db.query_row(
                "SELECT h.version,h.executable_hash,p.kind,p.position_encoding FROM semantic_requested_producers h JOIN semantic_producers p ON p.id=h.id AND p.version=h.version WHERE h.revision_id=?1 AND h.id=?2",
                params![selected.key,producer_id], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
            ).optional()?;
            let (version, executable_hash, kind, encoding) =
                descriptor.context(SelectedIntegrity(
                    "incompatible_index: selected producer descriptor missing".into(),
                ))?;
            let languages = semantic_ordered(
                db,
                "SELECT ordinal,language FROM semantic_requested_producer_languages WHERE revision_id=?1 AND id=?2 ORDER BY ordinal",
                &[&selected.key, &producer_id],
            )?;
            let producer = crate::semantic_evidence::Producer {
                id: producer_id.clone(),
                version,
                executable_hash,
                kind: semantic_enum(kind)?,
                position_encoding: semantic_enum(encoding)?,
                languages,
            };
            let selected_source_set: String = db.query_row(
                "SELECT source_set_id FROM native_revisions WHERE id=?1",
                [&selected.key],
                |r| r.get(0),
            )?;
            let requested_documents = db.prepare(
                "SELECT language,path FROM semantic_coverage WHERE revision_id=?1 AND producer_id=?2 AND requested=1 ORDER BY language,path"
            )?.query_map(params![selected.key,producer_id], |r| Ok(crate::native_evidence::DocumentKey { source_set_id:selected_source_set.clone(), language:r.get(0)?, path:r.get(1)? }))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let input = (|| -> Result<SelectedRawBatch> {
                let batch = db.query_row(
                "SELECT producer_id,source_set_id,captured_revision,source_manifest_hash,decoder_version,descriptor_bytes FROM semantic_raw_batches WHERE batch_id=?1",
                [batch_id], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?,r.get::<_, String>(2)?,r.get::<_, String>(3)?,r.get::<_, String>(4)?,r.get::<_, Vec<u8>>(5)?))
            ).optional()?;
                let (
                    raw_producer,
                    source_set_id,
                    captured_revision,
                    source_manifest_hash,
                    decoder_version,
                    descriptor_bytes,
                ) = batch.context(SelectedIntegrity(
                    "incompatible_index: selected batch missing".into(),
                ))?;
                selected_ensure!(
                    raw_producer == producer_id,
                    "incompatible_index: selected batch producer mismatch"
                );
                let descriptor =
                    crate::semantic_batch::descriptor::Descriptor::parse(&descriptor_bytes)
                        .map_err(|_| {
                            SelectedIntegrity(
                                "incompatible_index: selected batch descriptor".into(),
                            )
                        })?;
                let captures = db.prepare(
                    "SELECT capture_id,ordinal,language,artifact_hash,binary_bytes FROM semantic_raw_captures WHERE batch_id=?1 ORDER BY ordinal"
                )?.query_map([batch_id], |r| Ok(SelectedRawCapture {capture_id:r.get(0)?,ordinal:r.get(1)?,language:r.get(2)?,artifact_hash:r.get(3)?,binary_bytes:r.get(4)?}))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                selected_ensure!(
                    !captures.is_empty(),
                    "incompatible_index: selected capture missing"
                );
                let captured_revision_number: Option<i64> = db.query_row(
                "SELECT published_index_revision FROM native_revisions WHERE id=?1 AND native_revision_id=?2 AND source_set_id=?3",
                params![captured_key,captured_revision,source_set_id], |r| r.get(0)
            ).optional()?;
                let captured_revision_number = captured_revision_number.context(
                    SelectedIntegrity("incompatible_index: selected captured pin missing".into()),
                )?;
                let proofs = if metadata_only {
                    Vec::new()
                } else {
                    db.prepare(
                    "SELECT p.proof_id,p.record_kind,p.fact_digest,p.language,p.path,p.canonical_bytes,c.artifact_hash FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id=?1 ORDER BY p.proof_id"
                )?.query_map([batch_id], |r| Ok((r.get(0)?, StoredRawProof {record_kind:r.get(1)?,fact_digest:r.get(2)?,language:r.get(3)?,path:r.get(4)?,canonical_bytes:r.get(5)?,artifact_hash:r.get(6)?})))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                };
                #[cfg(feature = "test-support")]
                crate::semantic_import::note_selected_original_raw_proof_bytes(
                    proofs
                        .iter()
                        .map(|(_, proof): &(String, StoredRawProof)| proof.canonical_bytes.len())
                        .sum(),
                );

                let documents = db.prepare(
                    "SELECT d.language,d.path,d.source_hash,d.wire_digest,d.capture_id FROM semantic_raw_documents d JOIN semantic_raw_captures c ON c.capture_id=d.capture_id WHERE c.batch_id=?1 ORDER BY d.language,d.path"
                )?.query_map([batch_id], |r| Ok(((r.get(0)?,r.get(1)?),(r.get(2)?,r.get(3)?,r.get(4)?))))?
                    .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
                let capped_documents = db.prepare(
                    "SELECT d.language,d.path FROM semantic_raw_documents d JOIN semantic_raw_captures c ON c.capture_id=d.capture_id WHERE c.batch_id=?1 AND d.capped=1"
                )?.query_map([batch_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<rusqlite::Result<crate::scip_raw::CappedDocuments>>()?;
                let exclusions = db.prepare(
                    "SELECT language,path,reason FROM semantic_raw_exclusions WHERE batch_id=?1 ORDER BY language,path"
                )?.query_map([batch_id], |r| Ok(crate::scip_raw::Exclusion {language:r.get(0)?,relative_path:r.get(1)?,reason:r.get(2)?}))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(SelectedRawBatch {
                    producer: producer.clone(),
                    requested_documents: requested_documents.clone(),
                    producer_id,
                    source_set_id,
                    captured_revision,
                    batch_id,
                    descriptor,
                    source_manifest_hash,
                    decoder_version,
                    captures,
                    exclusions,
                    capped_documents,
                    proofs,
                    documents,
                    captured_pin: IndexPin {
                        index_generation: pin.index_generation,
                        index_revision: u64::try_from(captured_revision_number)?,
                    },
                })
            })();
            let input = match input {
                Err(error) if rejoin_storage_failure(&error) => {
                    return Err(rejoin_storage_error(error));
                }
                other => other,
            };
            inputs.push((producer, requested_documents, input));
        }
        Ok(inputs)
    }

    fn selected_native_inputs_at(
        &self,
        pin: IndexPin,
        loaded_docs: Option<&BTreeSet<crate::native_evidence::DocumentKey>>,
    ) -> Result<Vec<SelectedNativeInput>> {
        self.native_at_reconcile(pin, |db, selected| {
            self.selected_native_inputs_from_db(db, selected, pin, loaded_docs)
                .map_err(rejoin_storage_error)
        })
    }

    fn selected_native_inputs_from_db(
        &self,
        db: &Connection,
        selected: &ReadRevision,
        pin: IndexPin,
        loaded_docs: Option<&BTreeSet<crate::native_evidence::DocumentKey>>,
    ) -> Result<Vec<SelectedNativeInput>> {
        self.selected_raw_input_results_from_db(db,selected,pin,loaded_docs.is_some())?
            .into_iter()
            .map(|(producer, requested_documents, raw)| {
                let raw = match raw {
                    Err(error) if rejoin_storage_failure(&error) => return Err(rejoin_storage_error(error)),
                    other => other,
                };
                let proof_identities = if let (Some(_),Ok(input)) = (loaded_docs,&raw) {
                    db.prepare("SELECT p.proof_id,p.record_kind,p.fact_digest,p.language,p.path,c.artifact_hash FROM semantic_raw_proofs p JOIN semantic_raw_captures c ON c.capture_id=p.capture_id WHERE c.batch_id=?1 ORDER BY p.proof_id")?
                        .query_map([input.batch_id],|r|Ok(crate::scip_raw::RawProofIdentity {
                            proof_id:r.get(0)?,record_kind:r.get(1)?,fact_digest:r.get(2)?,
                            language:r.get(3)?,path:r.get(4)?,artifact_hash:r.get(5)?
                        }))?.collect::<rusqlite::Result<Vec<_>>>()?
                } else {Vec::new()};
                let captured = match &raw {
                    Ok(input) => {
                        let captured_pin=ReadRevision::new(input.captured_pin,semantic_pin_key(input.captured_pin));
                        let (native, sources) = self.retained_native_snapshot_from_db(db,&captured_pin,loaded_docs)?;
                        let components = self.retained_native_components_from_db(db,&captured_pin)?;
                        Ok((native,sources,components))
                    },
                    Err(error) => Err(SelectedIntegrity(format!(
                        "incompatible_index: selected capture unavailable: {error:#}"
                    ))
                    .into()),
                };
                Ok(SelectedNativeInput {
                    lightweight: loaded_docs.is_some(),
                    proof_identities,
                    producer,
                    requested_documents,
                    raw,
                    captured,
                })
            })
            .collect()
    }

    /// Materialize the complete normalized semantic tree owned by this selected pin.
    pub fn semantic_at(&self, pin: IndexPin) -> Result<crate::semantic_evidence::SemanticSnapshot> {
        self.native_at(pin, |db, selected| read_semantic(db, &selected.key))
    }

    /// Return only the latest eligible retained tuple for a failed or omitted
    /// selected producer/document, with declaration-keyed facts at their own pin.
    pub fn semantic_history_for(
        &self,
        pin: IndexPin,
        producer_id: &str,
        document: &crate::native_evidence::DocumentKey,
        syntax_id: &str,
    ) -> Result<Option<SemanticHistory>> {
        ensure!(
            id_codec::encode_sid(syntax_id).is_some(),
            "invalid syntax ID"
        );
        self.native_at(pin, |db, selected| {
            read_semantic_history(
                db,
                &selected.key,
                pin.index_revision,
                producer_id,
                document,
                syntax_id,
            )
        })
    }

    /// Set a deterministic UTC and monotonic clock for an isolated test store.
    /// The instant and all subsequent publications use this same source.
    #[doc(hidden)]
    pub fn set_retention_clock_for_tests(&self, wall: i64, monotonic: u64) {
        let mut clock = self.retention_clock.lock().unwrap();
        if clock.injected.is_none() {
            clock.origin_wall = wall;
            clock.origin_mono = monotonic;
        }
        clock.injected = Some((wall, monotonic));
    }

    fn retention_time(&self) -> Result<Duration> {
        let (wall, _) = self.retention_clock.lock().unwrap().sample()?;
        ensure!(
            (0..=9_007_199_254_740_991).contains(&wall),
            "invalid_supersession_time"
        );
        Ok(Duration::from_secs(wall as u64))
    }

    /// Leader-owned, best-effort derived GC. The caller chooses the hourly
    /// check cadence; the shared cache stamp enforces the daily scan limit.
    pub fn automatic_gc(&self, leader: &topology::WriterGuard) -> Result<usize> {
        let now = publication_second()?;
        self.automatic_gc_at(leader, now)
    }

    pub fn automatic_gc_cooperative(
        &self,
        leader: &topology::WriterGuard,
        hook: &mut dyn FnMut(topology::GcStage) -> Result<()>,
    ) -> Result<usize> {
        leader.belongs_to(&self.identity)?;
        leader.verify()?;
        self.identity.verify()?;
        self.roots
            .automatic_gc_at_with_hook(&self.identity, leader, publication_second()?, hook)
    }

    #[doc(hidden)]
    pub fn automatic_gc_at(&self, leader: &topology::WriterGuard, now: i64) -> Result<usize> {
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        self.roots.automatic_gc_at(&self.identity, leader, now)
    }

    /// Monotonic count of confirmed SQLite BUSY/LOCKED maintenance attempts.
    /// Interrupts, unknown queue probes and use-lock contention are excluded.
    pub fn maintenance_sqlite_busy_attempts(&self) -> u64 {
        self.maintenance_busy_attempts.load(Ordering::Relaxed)
    }

    fn record_maintenance_busy(&self, error: &anyhow::Error) {
        if maintenance_sqlite_busy(error) {
            self.maintenance_busy_attempts
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Oldest eligible retained or pending debt age. The no-work path uses a
    /// covering MAX, one partial-index LIMIT 1 and one indexed debt LIMIT 1.
    /// Only if debt exists do we scan that debt set to find its oldest stamp.
    pub fn oldest_due_debt_age(&self, leader: &topology::WriterGuard) -> Result<Option<Duration>> {
        let observed = (|| -> Result<Option<Duration>> {
            leader.belongs_to(&self.identity)?;
            leader.verify()?;
            self.identity.verify()?;
            let (wall, elapsed, origin) = {
                let clock = self.retention_clock.lock().unwrap();
                let (wall, mono) = clock.sample()?;
                ensure!(
                    (0..=9_007_199_254_740_991).contains(&wall),
                    "invalid_supersession_time"
                );
                (
                    wall,
                    mono.saturating_sub(clock.origin_mono),
                    clock.origin_wall,
                )
            };
            let guard = self.roots.index_use_existing_readonly(&self.identity)?;
            let db = open_index_marker_probe(
                &self.roots.index_db(&self.identity),
                false,
                Duration::ZERO,
            )?;
            storage_result(db.execute_batch("BEGIN DEFERRED"))?;
            begin_read_snapshot(&db, &self.roots.index_db(&self.identity), Duration::ZERO)?;
            let latest: Option<i64> = db.query_row(
                "SELECT max(superseded_at) FROM native_revision_supersessions",
                [],
                |r| r.get(0),
            )?;
            let now = wall.max(latest.unwrap_or(wall));
            if !retention_clock_plausible(origin, now, elapsed) {
                return Ok(None);
            }
            let pending: Option<String> = db
                .query_row(
                    "SELECT revision_id FROM native_revision_release_debt LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let pending_oldest: Option<i64> = if pending.is_some() {
                db.query_row(
                    "SELECT min(s.superseded_at) FROM native_revision_release_debt d
                JOIN native_revision_supersessions s ON s.revision_id=d.revision_id",
                    [],
                    |r| r.get(0),
                )?
            } else {
                None
            };
            let due_oldest: Option<i64> = db
                .query_row(
                    "SELECT superseded_at FROM native_revision_supersessions
             WHERE state='retained' AND superseded_at<=?1
             ORDER BY superseded_at,revision_id LIMIT 1",
                    [now.saturating_sub(900)],
                    |r| r.get(0),
                )
                .optional()?;
            guard.verify()?;
            self.identity.verify()?;
            Ok(pending_oldest
                .into_iter()
                .chain(due_oldest)
                .min()
                .map(|stamp| Duration::from_secs(now.saturating_sub(stamp) as u64)))
        })();
        match observed {
            Err(error) if maintenance_sqlite_busy(&error) => {
                self.record_maintenance_busy(&error);
                Err(SqliteContention(()).into())
            }
            result => result,
        }
    }

    pub fn maintenance_oldest_due_age_secs(
        &self,
        leader: &topology::WriterGuard,
    ) -> Result<Option<u64>> {
        Ok(self.oldest_due_debt_age(leader)?.map(|age| age.as_secs()))
    }

    /// Pause after a new lock incarnation is durable but before validated
    /// predecessor metadata is rebound. Integration tests use this seam only.
    #[doc(hidden)]
    pub fn set_writer_before_metadata_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        self.writer_before_metadata_hook.set(hook);
    }

    /// Lower the publication page cache so a test publication spills dirty
    /// pages before COMMIT, as a publication larger than the cache does.
    #[doc(hidden)]
    pub fn set_publication_cache_kib_for_tests(&self, kib: i64) {
        self.publication_cache_kib.store(kib, Ordering::Release);
    }

    /// Pause a publication right after it clears its readers-blocked mark.
    #[cfg(test)]
    fn set_after_readers_unblocked_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        self.publication_gate.test_after_readers_unblocked.set(hook);
    }

    /// Deterministic rollback after a complete index rebuild but before COMMIT.
    #[doc(hidden)]
    pub fn set_publication_before_commit_hook_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.publication_before_commit_hook.set(hook);
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_publication_before_semantic_hook_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.publication_before_semantic_hook.set(hook);
    }

    /// Deterministic test barrier just before writer admission. Never use in production.
    #[doc(hidden)]
    pub fn set_maintenance_before_writer_hook_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.maintenance_before_writer_hook.set(hook);
    }

    /// Deterministic test barrier after the first real DELETE inside IMMEDIATE.
    #[doc(hidden)]
    pub fn set_maintenance_after_first_delete_hook_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.maintenance_after_first_delete_hook.set(hook);
    }

    /// One small writer unit. All negative no-op probes stay read-only and indexed.
    /// A newly waiting publisher or changed durable FIFO causes rollback.
    pub fn maintenance_step(
        &self,
        leader: &topology::WriterGuard,
        permit: &MaintenancePermit,
        probe: &QueueProbeAdmission,
        mut priority: impl FnMut() -> bool,
    ) -> Result<MaintenanceOutcome> {
        if !self.maintenance_priority(permit, probe, &mut priority) {
            return Ok(MaintenanceOutcome::Deferred);
        }
        leader.belongs_to(&self.identity)?;
        leader.verify()?;
        self.identity.verify()?;
        let (wall, elapsed, origin) = {
            let clock = self.retention_clock.lock().unwrap();
            let (wall, mono) = clock.sample()?;
            ensure!(
                (0..=9_007_199_254_740_991).contains(&wall),
                "invalid_supersession_time"
            );
            (
                wall,
                mono.saturating_sub(clock.origin_mono),
                clock.origin_wall,
            )
        };
        // The reader holds a nonblocking SH use guard and never opens an IMMEDIATE
        // transaction on the no-work path. The covering and partial indexes make
        // the probe independent of workspace size and history size.
        let guard = match self.roots.index_use_existing_readonly(&self.identity) {
            Ok(guard) => guard,
            Err(error) if maintenance_contention(&error) => {
                self.record_maintenance_busy(&error);
                return Ok(MaintenanceOutcome::Deferred);
            }
            Err(error) => return Err(error),
        };
        let path = self.roots.index_db(&self.identity);
        let db = match open_index_marker_probe(&path, false, Duration::ZERO) {
            Ok(db) => db,
            Err(error) if maintenance_contention(&error) => {
                self.record_maintenance_busy(&error);
                return Ok(MaintenanceOutcome::Deferred);
            }
            Err(error) => return Err(error),
        };
        let snapshot = storage_result(db.execute_batch("BEGIN DEFERRED"))
            .and_then(|()| begin_read_snapshot(&db, &path, Duration::ZERO));
        if let Err(error) = snapshot {
            if maintenance_contention(&error) {
                self.record_maintenance_busy(&error);
                return Ok(MaintenanceOutcome::Deferred);
            }
            return Err(error);
        }
        let selected_read = (|| -> Result<(IndexPin, i64, Option<String>)> {
            let head = ReadRevision::current(&db)?.pin;
            let max_stamp: Option<i64> = db.query_row(
                "SELECT max(superseded_at) FROM native_revision_supersessions",
                [],
                |r| r.get(0),
            )?;
            let now = wall.max(max_stamp.unwrap_or(wall));
            let pending: Option<String> = db
                .query_row(
                    "SELECT revision_id FROM native_revision_release_debt LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .optional()?;
            let due = if retention_clock_plausible(origin, now, elapsed) {
                db.query_row(
                    "SELECT revision_id FROM native_revision_supersessions
                    WHERE state='retained' AND superseded_at<=?1
                    AND NOT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s WHERE s.captured_pin=native_revision_supersessions.revision_id)
                    ORDER BY superseded_at,revision_id LIMIT 1",
                    [now.saturating_sub(900)],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
            } else {
                None
            };
            Ok((head, now, pending.or(due)))
        })();
        drop(db);
        guard.verify()?;
        drop(guard);
        let (head, now, selected) = match selected_read {
            Ok(read) => read,
            Err(error) if maintenance_contention(&error) => {
                self.record_maintenance_busy(&error);
                return Ok(MaintenanceOutcome::Deferred);
            }
            Err(error) => return Err(error),
        };
        let Some(selected) = selected else {
            return Ok(MaintenanceOutcome::Idle);
        };
        self.maintenance_before_writer_hook.run();
        if !self.maintenance_priority(permit, probe, &mut priority) {
            return Ok(MaintenanceOutcome::Deferred);
        }
        let guard = match self.roots.index_use_existing_readonly(&self.identity) {
            Ok(guard) => guard,
            Err(error) if maintenance_contention(&error) => {
                self.record_maintenance_busy(&error);
                return Ok(MaintenanceOutcome::Deferred);
            }
            Err(error) => return Err(error),
        };
        let mut db = match open_index_marker_probe(&path, true, Duration::ZERO) {
            Ok(db) => db,
            Err(error) if maintenance_contention(&error) => {
                self.record_maintenance_busy(&error);
                return Ok(MaintenanceOutcome::Deferred);
            }
            Err(error) => return Err(error),
        };
        if !self.maintenance_priority(permit, probe, &mut priority) {
            return Ok(MaintenanceOutcome::Deferred);
        }
        let progress = MaintenanceProgress {
            gate: &self.publication_gate,
        };
        let progress_guard = install_maintenance_progress(&db, &progress);
        let work = (|| -> Result<MaintenanceOutcome> {
            if !progress.check() {
                return Ok(MaintenanceOutcome::Deferred);
            }
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            // Targeted header and state reads only. No broad status/manifest sweep.
            let current = ReadRevision::current(&tx)?;
            ensure!(
                current.pin == head,
                "revision conflict: maintenance head changed"
            );
            ensure!(
                selected != current.key,
                "revision conflict: maintenance selected head"
            );
            let state: String = tx.query_row(
                "SELECT state FROM native_revision_supersessions WHERE revision_id=?1",
                [&selected],
                |r| r.get(0),
            )?;
            if state == "retained" {
                let changed = tx.execute(
                    "UPDATE native_revision_supersessions SET state='pending'
                    WHERE revision_id=?1 AND state='retained' AND superseded_at<=?2
                    AND NOT EXISTS(SELECT 1 FROM semantic_revision_raw_selections s WHERE s.captured_pin=native_revision_supersessions.revision_id)",
                    params![selected, now.saturating_sub(900)],
                )?;
                ensure!(changed == 1, "revision conflict: due revision changed");
                tx.execute(
                    "INSERT INTO native_revision_release_debt(revision_id,phase)
                    VALUES(?1,'pending')",
                    [&selected],
                )?;
                tx.execute(
                    "INSERT INTO revision_capture_inputs(revision_id,input_key,payload)
                    VALUES(?1,'__pending_release:v1','pending_release:v1')",
                    [&selected],
                )?;
            } else {
                selected_ensure!(
                    state == "pending",
                    "incompatible_index: invalid pending debt state"
                );
                let semantic_progress = release_semantic_tuple(&tx, &selected)?;
                let entries: Vec<(i64, i64, Option<i64>, i64)> = if semantic_progress {
                    Vec::new()
                } else {
                    tx.prepare(
                        "SELECT vid,gid,cid,ordinal
                     FROM revision_documents WHERE revision_id=?1 ORDER BY ordinal LIMIT 64",
                    )?
                    .query_map([&selected], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?
                };
                for (version, graph, class, ordinal) in &entries {
                    tx.execute(
                        "INSERT OR IGNORE INTO native_release_candidate_versions VALUES(?1,?2)",
                        params![selected, version],
                    )?;
                    tx.execute(
                        "INSERT OR IGNORE INTO native_release_candidate_graphs VALUES(?1,?2)",
                        params![selected, graph],
                    )?;
                    if let Some(class) = class {
                        tx.execute(
                            "INSERT OR IGNORE INTO native_release_candidate_classes VALUES(?1,?2)",
                            params![selected, class],
                        )?;
                    }
                    let deleted = tx.execute(
                        "DELETE FROM revision_documents WHERE revision_id=?1 AND ordinal=?2",
                        params![selected, ordinal],
                    )?;
                    if deleted == 1 && ordinal == &entries[0].3 {
                        self.maintenance_after_first_delete_hook.run();
                    }
                }
                // The candidate journal and manifest DELETE commit atomically.
                // Bound dependent cleanup by 64 candidates per table per unit.
                cleanup_release_candidate(&tx, &selected, "class")?;
                cleanup_release_candidate(&tx, &selected, "graph")?;
                cleanup_release_candidate(&tx, &selected, "version")?;
                let has_manifest: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?1 LIMIT 1)",
                    [&selected],
                    |r| r.get(0),
                )?;
                let has_candidates: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_release_candidate_classes WHERE revision_id=?1)
                     OR EXISTS(SELECT 1 FROM native_release_candidate_graphs WHERE revision_id=?1)
                     OR EXISTS(SELECT 1 FROM native_release_candidate_versions WHERE revision_id=?1)",
                    [&selected], |r| r.get(0))?;
                let has_semantic: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM semantic_coverage WHERE revision_id=?1)
                        OR EXISTS(SELECT 1 FROM semantic_revision_producers WHERE revision_id=?1)
                        OR EXISTS(SELECT 1 FROM semantic_requested_producers WHERE revision_id=?1)
                        OR EXISTS(SELECT 1 FROM semantic_revision_raw_selections WHERE revision_id=?1)",
                    [&selected],
                    |row| row.get(0),
                )?;
                if !has_manifest && !has_candidates && !has_semantic {
                    tx.execute(
                        "DELETE FROM revision_capture_inputs WHERE revision_id=?1",
                        [&selected],
                    )?;
                    tx.execute(
                        "INSERT INTO revision_capture_inputs(revision_id,input_key,payload)
                        VALUES(?1,'__released:v1','released:v1')",
                        [&selected],
                    )?;
                    tx.execute(
                        "DELETE FROM native_revision_release_debt WHERE revision_id=?1",
                        [&selected],
                    )?;
                    tx.execute(
                        "UPDATE native_revision_supersessions SET state='released'
                        WHERE revision_id=?1 AND state='pending'",
                        [&selected],
                    )?;
                }
            }
            leader.verify()?;
            self.identity.verify()?;
            guard.verify()?;
            if !progress.check() || !self.maintenance_priority(permit, probe, &mut priority) {
                return Ok(MaintenanceOutcome::Deferred);
            }
            tx.commit()?;
            Ok(MaintenanceOutcome::Progress)
        })();
        drop(progress_guard);
        // Dropping an uncommitted Transaction rolls back BEFORE releasing the
        // writer connection, including BUSY COMMIT in DELETE journal mode.
        match work {
            Err(error) if maintenance_contention(&error) => {
                self.record_maintenance_busy(&error);
                Ok(MaintenanceOutcome::Deferred)
            }
            other => other,
        }
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn maintain_one_revision_step_for_tests(
        &self,
        leader: &topology::WriterGuard,
    ) -> Result<bool> {
        let permit = self
            .maintenance_try_enter()
            .context("maintenance admission unavailable")?;
        let probe = self.open_maintenance_queue_probe()?;
        Ok(matches!(
            self.maintenance_step(leader, &permit, &probe, || true)?,
            MaintenanceOutcome::Progress
        ))
    }

    /// Explicit leader cleanup for CLI/test callers. Daemon maintenance uses
    /// one `maintenance_step` at a time, never this draining convenience API.
    /// Explicit release is allowed to finish a revision larger than 4096 units;
    /// publication priority can still defer any individual unit.
    pub fn maintain_revisions(&self, leader: &topology::WriterGuard) -> Result<()> {
        let Some(permit) = self.maintenance_try_enter() else {
            return Ok(());
        };
        loop {
            let probe = self.open_maintenance_queue_probe()?;
            match self.maintenance_step(leader, &permit, &probe, || true)? {
                MaintenanceOutcome::Progress => {}
                MaintenanceOutcome::Idle | MaintenanceOutcome::Deferred => return Ok(()),
            }
        }
    }

    /// Public release is due-fenced. Earlier release is never exposed to a
    /// production caller; tests can advance the injected clock instead.
    pub fn release_revision(&self, pin: IndexPin, leader: &topology::WriterGuard) -> Result<()> {
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        let clock = self.retention_clock.lock().unwrap();
        let (wall, mono) = clock.sample()?;
        let elapsed = mono.saturating_sub(clock.origin_mono);
        let origin = clock.origin_wall;
        drop(clock);
        let db = self.cache()?;
        storage_result(db.execute_batch("BEGIN DEFERRED"))?;
        begin_read_snapshot(
            &db,
            &self.roots.index_db(&self.identity),
            Duration::from_secs(5),
        )?;
        let head = ReadRevision::current(&db)?.pin;
        ensure!(
            pin.index_generation == head.index_generation
                && pin.index_revision > 0
                && pin.index_revision < head.index_revision,
            "revision conflict: cannot release head or foreign pin"
        );
        let key = format!("pin:v1:{}:{}", pin.index_generation, pin.index_revision);
        let entry: Option<(i64,String)> = db.query_row(
            "SELECT superseded_at,state FROM native_revision_supersessions WHERE revision_id=?1",
            [&key], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        let (stamp, state) = entry.context("revision conflict: missing superseded native pin")?;
        let max_stamp: Option<i64> = db.query_row(
            "SELECT max(superseded_at) FROM native_revision_supersessions",
            [],
            |r| r.get(0),
        )?;
        let now = wall.max(max_stamp.unwrap_or(wall));
        ensure!(
            state == "pending"
                || state == "released"
                || (retention_clock_plausible(origin, now, elapsed) && retention_due(now, stamp)),
            "revision conflict: native pin is within retention grace"
        );
        drop(db);
        self.maintain_revisions(leader)?;
        // Draining is best-effort: admission may be occupied, a step may defer,
        // or the bounded loop may end with durable debt. The explicit release
        // API must only report success after the requested pin is committed.
        let observed = (|| -> Result<bool> {
            let guard = self.roots.index_use_existing_readonly(&self.identity)?;
            let db = open_index_marker_probe(
                &self.roots.index_db(&self.identity),
                false,
                Duration::ZERO,
            )?;
            storage_result(db.execute_batch("BEGIN DEFERRED"))?;
            begin_read_snapshot(&db, &self.roots.index_db(&self.identity), Duration::ZERO)?;
            let exact: Option<(String, i64, i64, i64)> = db
                .query_row(
                    "SELECT state,
                        (SELECT count(*) FROM native_revision_release_debt WHERE revision_id=?1),
                        (SELECT count(*) FROM revision_capture_inputs WHERE revision_id=?1
                            AND input_key='__released:v1' AND payload='released:v1'),
                        (SELECT count(*) FROM revision_capture_inputs WHERE revision_id=?1
                            AND input_key='__pending_release:v1')
                     FROM native_revision_supersessions WHERE revision_id=?1",
                    [&key],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            leader.verify()?;
            self.identity.verify()?;
            guard.verify()?;
            Ok(matches!(exact, Some((state, 0, 1, 0)) if state == "released"))
        })();
        match observed {
            Ok(true) => Ok(()),
            Ok(false) => Err(SqliteContention(()).into()),
            Err(error) if maintenance_sqlite_busy(&error) => Err(SqliteContention(()).into()),
            Err(error) => Err(error),
        }
    }

    fn check_retained_foreign_keys(db: &Connection) -> Result<()> {
        selected_ensure!(
            db.prepare("PRAGMA foreign_key_check")?
                .query([])?
                .next()?
                .is_none(),
            "incompatible_index: retained reference violates foreign key"
        );
        Ok(())
    }

    /// Collect only trees with no references from ANY remaining manifest.
    /// All dependent rows and the FK check share the fenced immediate transaction.
    pub fn collect_unreferenced(&self, leader: &topology::WriterGuard) -> Result<()> {
        leader.belongs_to(&self.identity)?;
        self.identity.verify()?;
        self.with_evidence(|db| {
            self.read_revision(db, None)?;
            Ok(())
        })?;
        let mut db = self.cache_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        self.read_revision(&tx, None)?;
        leader.verify()?;
        let unreferenced_class = "SELECT p.cid FROM class_projections p WHERE NOT EXISTS
            (SELECT 1 FROM revision_documents m WHERE m.cid=p.cid)
            AND NOT EXISTS (SELECT 1 FROM native_release_candidate_classes q WHERE q.cid=p.cid)";
        for table in ["class_relations", "classes"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE cid IN ({unreferenced_class})"),
                [],
            )?;
        }
        tx.execute(
            &format!("DELETE FROM class_projections WHERE cid IN ({unreferenced_class})"),
            [],
        )?;
        let unreferenced_graph = "SELECT p.gid FROM graph_projections p WHERE NOT EXISTS
            (SELECT 1 FROM revision_documents m WHERE m.gid=p.gid)
            AND NOT EXISTS (SELECT 1 FROM native_release_candidate_graphs q WHERE q.gid=p.gid)
            AND NOT EXISTS (SELECT 1 FROM class_projections c WHERE c.gid=p.gid)";
        for table in ["graph_display_labels", "graph_nodes"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE gid IN ({unreferenced_graph})"),
                [],
            )?;
        }
        tx.execute(
            &format!("DELETE FROM graph_projections WHERE gid IN ({unreferenced_graph})"),
            [],
        )?;
        let unreferenced_version = "SELECT v.vid FROM document_versions v WHERE NOT EXISTS
            (SELECT 1 FROM revision_documents m WHERE m.vid=v.vid)
            AND NOT EXISTS (SELECT 1 FROM native_release_candidate_versions q WHERE q.vid=v.vid)
            AND NOT EXISTS (SELECT 1 FROM graph_projections g WHERE g.vid=v.vid)";
        for table in [
            "native_version_call_regions",
            "native_version_references",
            "native_version_capability_gaps",
            "native_version_capability_families",
            "native_version_calls",
            "native_version_control_regions",
            "native_version_header_items",
            "native_version_parameters",
            "native_version_headers",
            "native_version_ancestor_signature_types",
            "native_version_own_signature_types",
            "native_version_declaration_ancestors",
            "native_version_search_cards",
            "native_version_declarations",
            "native_version_coverage_roles",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE vid IN ({unreferenced_version})"),
                [],
            )?;
        }
        tx.execute(
            &format!("DELETE FROM document_versions WHERE vid IN ({unreferenced_version})"),
            [],
        )?;
        Self::check_retained_foreign_keys(&tx)?;
        leader.verify()?;
        self.identity.verify()?;
        tx.commit()?;
        Ok(())
    }

    // SQL aggregates the selected path's ancillary native and class rows.
    // SourceFile JSON has its own envelope; derived graph bytes are checked
    // after source and native rows are decoded.
    fn selected_ancillary_byte_usage(
        db: &Connection,
        vid: i64,
        cid: i64,
        version_id: &str,
        source_set_id: &str,
    ) -> Result<(i64, i64)> {
        Ok(db.prepare_cached(SELECTED_ANCILLARY_BYTE_SQL)?.query_row(
            params![
                vid,
                cid,
                version_id.len() as i64,
                source_set_id.len() as i64
            ],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
    /// Read one selected document and project its graph once from the stored
    /// native rows.
    fn selected_graph_document_for(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
    ) -> Result<SelectedDocument> {
        self.fetch_selected_header(db, path, selected, None)
            .and_then(|header| self.fetch_selected_rows(db, header, selected, None))
            .and_then(Self::project_fetched)
    }
    /// Read one selected document in this snapshot: first its manifest row and
    /// the SQL row and byte budgets (`fetch_selected_header`), then, only after
    /// those budgets pass, the rows they bound (`fetch_selected_rows`). Stored
    /// rows are decoded as they are read; a row that does not decode fails
    /// typed. They are not compared with the source (#261).
    fn fetch_selected_header(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<SelectedHeader> {
        #[cfg(test)]
        count_selected_read(|counts| counts.document_reads += 1);
        use crate::native_evidence::DocumentKey;
        optional_hint_check(progress)?;
        let sql = "SELECT r.source_set_id,m.language,m.path,d.id,g.graph_hash,
            c.content_hash,d.content_hash,d.byte_length,length(d.source_bytes),
            g.graph_hash,g.state,g.class_extraction_state,length(CAST(g.class_extraction_payload AS BLOB)),
            c.content_hash,c.state,r.native_revision_id,d.vid,g.gid,c.cid
            FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id
            JOIN document_versions d ON d.vid=m.vid
              AND d.language=m.language AND d.path=m.path
            JOIN graph_projections g ON g.gid=m.gid
              AND g.vid=d.vid AND g.language=d.language
            LEFT JOIN class_projections c ON c.cid=m.cid AND c.gid=g.gid
            WHERE m.path=?1 AND m.revision_id=?2 LIMIT 2";
        let mut stmt = db.prepare_cached(sql)?;
        optional_hint_check(progress)?;
        let rows = stmt
            .query_map(params![path, selected.key], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    graph_id_row(r, 4)?,
                    class_id_row(r, 5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, String>(9)?,
                    r.get::<_, String>(10)?,
                    r.get::<_, String>(11)?,
                    r.get::<_, Option<i64>>(12)?,
                    r.get::<_, Option<String>>(13)?,
                    r.get::<_, Option<String>>(14)?,
                    r.get::<_, String>(15)?,
                    r.get::<_, i64>(16)?,
                    r.get::<_, i64>(17)?,
                    r.get::<_, Option<i64>>(18)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        selected_ensure!(
            rows.len() == 1,
            "incompatible_index: selected document missing or ambiguous"
        );
        let (
            source_set,
            language,
            stored_path,
            version_id,
            graph_id,
            class_id,
            hash,
            length,
            bytes,
            graph_hash,
            graph_state,
            class_state,
            f_length,
            class_hash,
            class_projection_state,
            native_revision_id,
            vid,
            gid,
            cid,
        ) = rows.into_iter().next().unwrap();
        selected_ensure!(
            stored_path == path
                && source_set == format!("source-set:v1:{}", self.root_id())
                && length == bytes
                && (0..=256 * 1024 * 1024).contains(&bytes)
                && [
                    &source_set,
                    &language,
                    &stored_path,
                    &version_id,
                    &graph_id,
                    &hash,
                    &graph_hash,
                    &native_revision_id
                ]
                .iter()
                .all(|value| value.len() <= 16 * 1024),
            "incompatible_index: selected source byte budget exceeded"
        );
        let class_id = class_id.context(SelectedIntegrity(
            "incompatible_index: selected class projection missing".into(),
        ))?;
        let cid = cid.context(SelectedIntegrity(
            "incompatible_index: selected class surrogate missing".into(),
        ))?;
        selected_ensure!(
            graph_state == "ready"
                && class_projection_state.as_deref() == Some("ready")
                && class_hash.is_some()
                && class_id.len() <= 16 * 1024
                && (if matches!(language.as_str(), "java" | "python") {
                    f_length.is_some_and(|n| (0..=64 * 1024 * 1024).contains(&n))
                } else {
                    f_length.is_none()
                }),
            "incompatible_index: selected class projection invalid"
        );
        selected_ensure!(
            matches!(
                (language.as_str(), class_state.as_str()),
                ("java" | "python", "ready") | ("javascript" | "rust", "notApplicable")
            ),
            "incompatible_index: class extraction state invalid"
        );
        let key = DocumentKey {
            source_set_id: source_set.clone(),
            language: language.clone(),
            path: path.into(),
        };
        let scope = v8_native_scope_for(db, &key, selected)?.context(SelectedIntegrity(
            "incompatible_index: selected native version missing".into(),
        ))?;
        selected_ensure!(
            scope.version_id == version_id && scope.native_revision_id == native_revision_id,
            "incompatible_index: selected native manifest mismatch"
        );
        let row_limit = bytes.saturating_mul(16).clamp(1024, 1_000_000);
        let counts_sql = "SELECT (SELECT count(*) FROM graph_nodes WHERE gid=?1),
            (SELECT count(*) FROM graph_display_labels WHERE gid=?1),
            (SELECT count(*) FROM native_version_calls WHERE vid=?3),
            (SELECT count(*) FROM native_version_control_regions WHERE vid=?3),
            (SELECT count(*) FROM classes WHERE cid=?2),
            (SELECT count(*) FROM class_relations WHERE cid=?2),
            (SELECT count(*) FROM native_version_coverage_roles WHERE vid=?3),
            (SELECT count(*) FROM native_version_declarations WHERE vid=?3),
            (SELECT count(*) FROM native_version_declaration_ancestors WHERE vid=?3),
            (SELECT count(*) FROM native_version_own_signature_types WHERE vid=?3),
            (SELECT count(*) FROM native_version_ancestor_signature_types WHERE vid=?3),
            (SELECT count(*) FROM native_version_headers WHERE vid=?3),
            (SELECT count(*) FROM native_version_header_items WHERE vid=?3),
            (SELECT count(*) FROM native_version_parameters WHERE vid=?3),
            (SELECT count(*) FROM native_version_call_regions WHERE vid=?3),
            (SELECT count(*) FROM native_version_references WHERE vid=?3),
            (SELECT count(*) FROM native_version_capability_families WHERE vid=?3),
            (SELECT count(*) FROM native_version_capability_gaps WHERE vid=?3)";
        let counts: Vec<i64> =
            db.prepare_cached(counts_sql)?
                .query_row(params![gid, cid, vid], |r| {
                    (0..18)
                        .map(|i| r.get(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })?;
        selected_ensure!(
            counts.iter().all(|&n| n >= 0 && n <= row_limit),
            "incompatible_index: selected evidence row budget exceeded"
        );
        optional_hint_check(progress)?;
        let (max_row, total_bytes) =
            Self::selected_ancillary_byte_usage(db, vid, cid, &version_id, &source_set)?;
        selected_ensure!(
            max_row >= 0
                && total_bytes >= 0
                && max_row
                    <= bytes
                        .saturating_mul(32)
                        .saturating_add(16 * 1024)
                        .min(256 * 1024 * 1024)
                && total_bytes
                    <= bytes
                        .saturating_mul(64)
                        .saturating_add(256 * 1024)
                        .min(512 * 1024 * 1024),
            "incompatible_index: selected evidence byte budget exceeded"
        );
        // The graph byte budget is checked on serialized derived rows after
        // the native facts and labels have been fetched. The row counts above
        // still bound their allocation before any native decoding.
        let stored_bytes = [bytes, total_bytes, f_length.unwrap_or(0)]
            .iter()
            .map(|&n| u64::try_from(n).unwrap_or(0))
            .sum();
        Ok(SelectedHeader {
            path: path.to_owned(),
            language,
            hash,
            bytes,
            scope,
            stored_bytes,
        })
    }
    fn fetch_selected_rows(
        &self,
        db: &Connection,
        header: SelectedHeader,
        selected: &ReadRevision,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<FetchedDocument> {
        self.fetch_selected_raw(db, header, selected, progress)?(progress)
    }

    /// `fetch_selected_rows` without decoding: read the source and every
    /// native row, and return the decoder that checks and assembles them.
    fn fetch_selected_raw(
        &self,
        db: &Connection,
        header: SelectedHeader,
        selected: &ReadRevision,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<impl Decode<FetchedDocument> + use<>> {
        let SelectedHeader {
            path,
            language,
            hash,
            bytes,
            scope,
            stored_bytes,
        } = header;
        let path = path.as_str();
        let file = Self::selected_source_row_bounded_for_inner(
            db,
            path,
            256 * 1024 * 1024,
            selected,
            progress,
        )?
        .context(SelectedIntegrity(
            "incompatible_index: selected source missing".into(),
        ))?;
        selected_ensure!(
            file.hash == hash && file.language == language && file.text.len() == bytes as usize,
            "incompatible_index: selected source identity mismatch"
        );
        let declarations = fetch_native_declarations_v8(db, &scope, progress)?;
        optional_hint_check(progress)?;
        let calls = fetch_native_calls_v8(db, &scope, progress)?;
        optional_hint_check(progress)?;
        let regions = fetch_native_control_regions_v8(db, &scope, progress)?;
        Ok(decoder(move |progress| {
            let declarations = declarations(progress)?;
            optional_hint_check(progress)?;
            let calls = calls(progress)?;
            optional_hint_check(progress)?;
            Ok(FetchedDocument {
                file,
                declarations,
                calls,
                regions: regions(progress)?,
                stored_bytes,
            })
        }))
    }

    /// Project one fetched document's graph from its stored native rows with
    /// the indexer's shared projector, within the graph byte budget.
    fn project_fetched(fetched: FetchedDocument) -> Result<SelectedDocument> {
        let FetchedDocument {
            file,
            declarations,
            calls,
            regions,
            stored_bytes: _,
        } = fetched;
        let (nodes, graph_calls, graph_regions) = crate::indexer::project_document(
            &file.path,
            file.text.as_bytes(),
            crate::indexer::NativeDocument {
                declarations: declarations.iter().collect(),
                calls: calls.iter().collect(),
                control_regions: regions.iter().collect(),
            },
        )
        .context(SelectedIntegrity(
            "incompatible_index: selected graph derivation failed".into(),
        ))?;
        derived_graph_bytes(&nodes, &graph_calls, &graph_regions, file.text.len() as i64).context(
            SelectedIntegrity("incompatible_index: selected graph byte budget exceeded".into()),
        )?;
        Ok(SelectedDocument {
            file,
            nodes,
            graph_calls,
            graph_regions,
        })
    }

    pub fn native_declarations_at(
        &self,
        pin: IndexPin,
        language: &str,
        lookup_key: &str,
    ) -> Result<Vec<crate::native_evidence::Declaration>> {
        self.native_at(pin, |db, selected| {
            Self::read_native_declarations_for(
                db,
                language,
                Some(lookup_key),
                None,
                false,
                selected,
            )
        })
    }
    fn read_native_declarations_for(
        db: &Connection,
        language: &str,
        lookup_key: Option<&str>,
        selected_path: Option<&str>,
        all_lookup_keys: bool,
        selected: &ReadRevision,
    ) -> Result<Vec<crate::native_evidence::Declaration>> {
        use crate::native_evidence::DocumentKey;
        let mut stmt = db.prepare(
            "SELECT DISTINCT r.source_set_id,m.language,m.path
            FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id
            JOIN native_version_declarations d ON d.vid=m.vid
            WHERE m.revision_id=?5 AND m.language=?1
              AND (?4 OR ((?2 IS NULL AND d.lookup_key IS NULL) OR d.lookup_key=?2))
              AND (?3 IS NULL OR m.path=?3) ORDER BY m.path LIMIT 5001",
        )?;
        let keys: Vec<DocumentKey> = stmt
            .query_map(
                params![
                    language,
                    lookup_key,
                    selected_path,
                    all_lookup_keys,
                    selected.key
                ],
                |r| {
                    Ok(DocumentKey {
                        source_set_id: r.get(0)?,
                        language: r.get(1)?,
                        path: r.get(2)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?;
        ensure!(
            keys.len() <= 5000,
            "incompatible_index: selected native lookup document budget exceeded"
        );
        let mut declarations = vec![];
        for key in keys {
            let scope = v8_native_scope_for(db, &key, selected)?.context(SelectedIntegrity(
                "incompatible_index: lookup manifest missing".into(),
            ))?;
            declarations.extend(read_native_declarations_v8(
                db,
                &scope,
                lookup_key,
                all_lookup_keys,
            )?);
        }
        declarations.sort_by(|a, b| a.syntax_id.cmp(&b.syntax_id));
        Ok(declarations)
    }

    /// Resolve an authenticated manifest's native revision to a retained historical pin.
    pub fn retained_native_pin_for(&self, revision: &str, source_set: &str) -> Result<IndexPin> {
        let generation = self.index_baseline()?.index_generation;
        self.with_evidence(|db| {
            let revision: i64 = db.query_row(
                "SELECT published_index_revision FROM native_revisions r WHERE native_revision_id=?1 AND source_set_id=?2 AND NOT EXISTS(SELECT 1 FROM revision_capture_inputs WHERE revision_id=r.id AND input_key IN ('__released:v1','__pending_release:v1')) ORDER BY published_index_revision DESC LIMIT 1",
                rusqlite::params![revision,source_set], |row|row.get(0),
            ).context("capture_unavailable: retained native revision missing")?;
            Ok(IndexPin { index_generation:generation, index_revision:u64::try_from(revision)? })
        })
    }

    /// The original canonical native root inputs retained with the selected revision.
    pub fn retained_native_components_at(
        &self,
        pin: IndexPin,
    ) -> Result<(
        serde_json::Value,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    )> {
        self.retained_native_components_at_with(pin, false)
    }

    fn retained_native_components_at_with(
        &self,
        pin: IndexPin,
        reconciling: bool,
    ) -> Result<(
        serde_json::Value,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    )> {
        let read = |db: &Connection, selected: &ReadRevision| {
            self.retained_native_components_from_db(db, selected)
        };
        if reconciling {
            self.native_at_reconcile(pin, read)
        } else {
            self.native_at(pin, read)
        }
    }

    fn retained_native_components_from_db(
        &self,
        db: &Connection,
        selected: &ReadRevision,
    ) -> Result<(
        serde_json::Value,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    )> {
        let (toolchain, config, dependencies): (String, String, String) = db.query_row(
            "SELECT retained_toolchain,retained_config,retained_dependencies FROM native_revisions WHERE id=?1",
            [&selected.key], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
        )?;
        let toolchain = serde_json::from_str(&toolchain).context(SelectedIntegrity(
            "incompatible_index: corrupt retained toolchain".into(),
        ))?;
        let config = serde_json::from_str(&config).context(SelectedIntegrity(
            "incompatible_index: corrupt retained config".into(),
        ))?;
        let dependencies = serde_json::from_str(&dependencies).context(SelectedIntegrity(
            "incompatible_index: corrupt retained dependencies".into(),
        ))?;
        Ok((toolchain, config, dependencies))
    }

    /// Captured source bytes and measured native facts from one retained pin, without disk reads.
    pub fn retained_native_snapshot_at(
        &self,
        pin: IndexPin,
    ) -> Result<(
        crate::native_evidence::Artifact,
        BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    )> {
        self.retained_native_snapshot_at_with(pin, false)
    }

    fn retained_native_snapshot_at_with(
        &self,
        pin: IndexPin,
        reconciling: bool,
    ) -> Result<(
        crate::native_evidence::Artifact,
        BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    )> {
        self.retained_native_snapshot_selected_at_with(pin, reconciling, None)
    }

    fn retained_native_snapshot_selected_at_with(
        &self,
        pin: IndexPin,
        reconciling: bool,
        loaded_docs: Option<&BTreeSet<crate::native_evidence::DocumentKey>>,
    ) -> Result<(
        crate::native_evidence::Artifact,
        BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    )> {
        let read = |db: &Connection, selected: &ReadRevision| {
            self.retained_native_snapshot_from_db(db, selected, loaded_docs)
        };
        if reconciling {
            self.native_at_reconcile(pin, read)
        } else {
            self.native_at(pin, read)
        }
    }

    fn retained_native_snapshot_from_db(
        &self,
        db: &Connection,
        selected: &ReadRevision,
        loaded_docs: Option<&BTreeSet<crate::native_evidence::DocumentKey>>,
    ) -> Result<(
        crate::native_evidence::Artifact,
        BTreeMap<crate::native_evidence::DocumentKey, Vec<u8>>,
    )> {
        use crate::native_evidence as n;
        #[cfg(feature = "test-support")]
        crate::semantic_batch::testing::note_whole_snapshot_read();
        let inventory: String = db.query_row(
            "SELECT source_inventory FROM native_revisions WHERE id=?1",
            [&selected.key],
            |r| r.get(0),
        )?;
        let documents: Vec<n::Document> = serde_json::from_str(&inventory).context(
            SelectedIntegrity("incompatible_index: corrupt retained native manifest".into()),
        )?;
        let NativeHeader {
            source_set,
            mut revision,
            producer,
        } = Self::read_native_header(db, selected)?;
        revision.documents = documents;
        let mut sources = BTreeMap::new();
        let (
            mut coverage,
            mut provenance,
            mut declarations,
            mut calls,
            mut control_regions,
            mut references,
            mut capability_reports,
            mut search_cards,
        ) = (
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
            vec![],
        );
        for document in &revision.documents {
            if loaded_docs.is_some_and(|loaded| !loaded.contains(&document.key)) {
                continue;
            }
            let rows = self.read_native_document(db, selected, document)?;
            sources.insert(document.key.clone(), rows.source);
            coverage.push(rows.coverage);
            provenance.push(rows.provenance);
            declarations.extend(rows.declarations);
            calls.extend(rows.calls);
            control_regions.extend(rows.control_regions);
            references.extend(rows.references);
            capability_reports.push(rows.capability_report);
            search_cards.extend(rows.search_cards);
        }
        Ok((
            n::Artifact {
                producer,
                source_set,
                revision,
                coverage,
                provenance,
                declarations,
                calls,
                control_regions,
                references,
                capability_reports,
                search_cards,
            },
            sources,
        ))
    }

    /// Header rows of a retained native revision. The document inventory is not read.
    fn read_native_header(db: &Connection, selected: &ReadRevision) -> Result<NativeHeader> {
        use crate::native_evidence as n;
        let (source_set_id, native_id, toolchain_hash, config_hash, dependency_hash): (String,String,String,String,String) = db.query_row("SELECT source_set_id,native_revision_id,toolchain_hash,config_hash,dependency_hash FROM native_revisions WHERE id=?1",[&selected.key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let (producer_id, producer_version): (String,String) = db.query_row("SELECT producer_id,producer_version FROM document_versions d JOIN revision_documents m ON m.vid=d.vid WHERE m.revision_id=?1 LIMIT 1",[&selected.key],|r|Ok((r.get(0)?,r.get(1)?)))?;
        let (executable_hash,kind,position_encoding): (String,String,String) = db.query_row("SELECT executable_hash,kind,position_encoding FROM native_producers WHERE id=?1 AND version=?2",params![producer_id,producer_version],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
        let languages = semantic_ordered(
            db,
            "SELECT ordinal,language FROM native_producer_languages WHERE producer_id=?1 AND producer_version=?2 ORDER BY ordinal",
            &[&producer_id, &producer_version],
        )?;
        let root_id: String = db.query_row(
            "SELECT root_id FROM native_source_sets WHERE id=?1",
            [&source_set_id],
            |r| r.get(0),
        )?;
        let dependencies = semantic_ordered(
            db,
            "SELECT ordinal,dependency_id FROM native_source_set_dependencies WHERE source_set_id=?1 ORDER BY ordinal",
            &[&source_set_id],
        )?;
        let source_set = n::SourceSet {
            id: source_set_id.clone(),
            root_id,
            languages: languages.clone(),
            dependencies,
        };
        let revision = n::Revision {
            id: native_id,
            source_set_id,
            documents: vec![],
            toolchain_hash,
            config_hash,
            dependency_hash,
        };
        Ok(NativeHeader {
            source_set,
            revision,
            producer: n::Producer {
                id: producer_id,
                version: producer_version,
                executable_hash,
                kind,
                languages,
                position_encoding,
            },
        })
    }

    /// One retained document's source bytes and native rows, checked against its inventory entry.
    fn read_native_document(
        &self,
        db: &Connection,
        selected: &ReadRevision,
        document: &crate::native_evidence::Document,
    ) -> Result<NativeDocumentRows> {
        self.read_native_document_rows(db, selected, document, true)
    }

    /// One document's native rows; without `complete`, its control regions and search cards
    /// are left empty (the staged join reads neither).
    fn read_native_document_rows(
        &self,
        db: &Connection,
        selected: &ReadRevision,
        document: &crate::native_evidence::Document,
        complete: bool,
    ) -> Result<NativeDocumentRows> {
        use crate::native_evidence as n;
        #[cfg(feature = "test-support")]
        crate::semantic_import::note_selected_original_native_row(&document.key);
        #[cfg(feature = "test-support")]
        crate::semantic_batch::testing::note_native_document_load();
        let scope = v8_native_scope_for(db, &document.key, selected)?.context(
            SelectedIntegrity("incompatible_index: retained native document absent".into()),
        )?;
        let source = self.read_native_source(db, selected, document)?;
        Ok(NativeDocumentRows {
            source,
            coverage: read_native_coverage_v8(db, &scope)?,
            provenance: n::Provenance {
                id: scope.proof_id(),
                producer_id: scope.producer_id.clone(),
                document: document.key.clone(),
                revision_id: scope.native_revision_id.clone(),
                content_hash: document.content_hash.clone(),
                evidence_kind: "measuredSyntax".into(),
                basis: None,
                freshness: "fresh".into(),
                derived_from: None,
            },
            declarations: read_native_declarations_v8(db, &scope, None, true)?,
            calls: read_native_calls_v8(db, &scope, None)?,
            control_regions: if complete {
                read_native_control_regions_v8(db, &scope, None)?
            } else {
                vec![]
            },
            references: read_native_references(db, &scope, None)?,
            capability_report: read_native_capability_report(db, &scope)?,
            search_cards: if complete {
                search::read_cards(db, scope.vid, &document.key.path)?
            } else {
                vec![]
            },
        })
    }

    /// The retained source bytes of one inventory entry, checked against its identity.
    fn read_native_source(
        &self,
        db: &Connection,
        selected: &ReadRevision,
        document: &crate::native_evidence::Document,
    ) -> Result<Vec<u8>> {
        let source = self
            .selected_source_row_for(db, &document.key.path, selected)?
            .context(SelectedIntegrity(
                "incompatible_index: retained source missing".into(),
            ))?;
        selected_ensure!(
            source.language == document.key.language
                && source.hash == document.content_hash
                && source.text.len() == document.byte_length,
            "incompatible_index: retained source identity mismatch"
        );
        #[cfg(feature = "test-support")]
        crate::semantic_import::note_selected_original_source_bytes(
            &document.key,
            source.text.len(),
        );
        Ok(source.text.into_bytes())
    }

    pub fn native_source_at(
        &self,
        pin: IndexPin,
        key: &crate::native_evidence::DocumentKey,
    ) -> Result<Option<(crate::native_evidence::Document, Vec<u8>)>> {
        use crate::native_evidence::Document;
        self.native_at(pin, |db, selected| {
            let Some(scope) = v8_native_scope_for(db, key, selected)? else {
                let path_exists: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?2 AND path=?1)",
                    params![key.path, selected.key],
                    |r| r.get(0),
                )?;
                ensure!(
                    !path_exists,
                    "incompatible_index: selected native source absent"
                );
                return Ok(None);
            };
            let source = self
                .selected_source_row_for(db, &key.path, selected)?
                .context(SelectedIntegrity("incompatible_index: paired graph source missing".into()))?;
            ensure!(
                source.path == key.path && source.language == key.language,
                "incompatible_index: native source identity differs from manifest"
            );
            let bytes = source.text.into_bytes();
            let document = Document {
                key: key.clone(),
                revision_id: scope.native_revision_id,
                content_hash: source.hash,
                byte_length: bytes.len(),
            };
            Ok(Some((document, bytes)))
        })
    }

    pub fn native_coverage_at(
        &self,
        pin: IndexPin,
        key: &crate::native_evidence::DocumentKey,
    ) -> Result<Option<crate::native_evidence::Coverage>> {
        self.native_at(pin, |db, selected| {
            Self::read_native_coverage_for(db, key, selected)
        })
    }
    fn read_native_coverage_for(
        db: &Connection,
        key: &crate::native_evidence::DocumentKey,
        selected: &ReadRevision,
    ) -> Result<Option<crate::native_evidence::Coverage>> {
        v8_native_scope_for(db, key, selected)?
            .map(|scope| read_native_coverage_v8(db, &scope))
            .transpose()
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn native_capability_report_at(
        &self,
        pin: IndexPin,
        key: &crate::native_evidence::DocumentKey,
    ) -> Result<Option<crate::native_evidence::CapabilityReport>> {
        self.native_at(pin, |db, selected| {
            v8_native_scope_for(db, key, selected)?
                .map(|scope| read_native_capability_report(db, &scope))
                .transpose()
        })
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn native_references_at(
        &self,
        pin: IndexPin,
        owner: &str,
    ) -> Result<Vec<crate::native_evidence::Reference>> {
        self.native_at(pin, |db, selected| {
            if let Some(scope) = v8_owner_scope_for(db, owner, selected)? {
                read_native_references(db, &scope, Some(owner))
            } else {
                let dangling: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_version_references c JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?2 WHERE c.owner_syntax_id=?1)",
                    params![id_codec::encode_sid(owner), selected.key], |r| r.get(0))?;
                selected_ensure!(!dangling, "incompatible_index: native owner absent");
                Ok(vec![])
            }
        })
    }

    pub fn native_calls_at(
        &self,
        pin: IndexPin,
        owner: &str,
    ) -> Result<Vec<crate::native_evidence::Call>> {
        self.native_at(pin, |db, selected| {
            if let Some(scope) = v8_owner_scope_for(db, owner, selected)? {
                read_native_calls_v8(db, &scope, Some(owner))
            } else {
                let dangling: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_version_calls c JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?2 WHERE c.owner_syntax_id=?1)",
                    params![id_codec::encode_sid(owner), selected.key],
                    |r| r.get(0),
                )?;
                selected_ensure!(!dangling, "incompatible_index: native owner absent");
                Ok(vec![])
            }
        })
    }
    pub fn native_control_regions_at(
        &self,
        pin: IndexPin,
        owner: &str,
    ) -> Result<Vec<crate::native_evidence::ControlRegion>> {
        self.native_at(pin, |db, selected| {
            if let Some(scope)=v8_owner_scope_for(db,owner,selected)? {
                read_native_control_regions_v8(db,&scope,Some(owner))
            } else {
                let dangling:bool=db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_version_control_regions c JOIN revision_documents m ON m.vid=c.vid AND m.revision_id=?2 WHERE c.owner_syntax_id=?1)",
                    params![id_codec::encode_sid(owner), selected.key],|r|r.get(0),
                )?;
                selected_ensure!(!dangling,"incompatible_index: native owner absent");
                Ok(vec![])
            }
        })
    }
    /// Search only the persisted projection. Wildcards are literal user text.
    /// One read snapshot and revision guard; no source reads or catalog rebuilds.
    pub fn navigation_at(
        &self,
        request: &crate::navigation::NavigationRequest,
    ) -> Result<crate::navigation::NavigationResult> {
        self.with_evidence(|tx| self.navigation_in(tx, request, false))
    }
    fn navigation_in(
        &self,
        tx: &Connection,
        request: &crate::navigation::NavigationRequest,
        pre_startup: bool,
    ) -> Result<crate::navigation::NavigationResult> {
        request.validate()?;
        let selected =
            self.read_revision_for(tx, Some(request.expected_revision()), pre_startup)?;
        let revision = selected.pin;
        // Policy selection is local to this exact selected snapshot, never the head.
        let mut producer_bytes = Vec::new();
        let mut multiple_producers = false;
        let effective_request = match request {
            crate::navigation::NavigationRequest::Source(selector)
                if selector.semantic_producer_policy.is_some() =>
            {
                let producers = tx.prepare(
                    "SELECT CASE WHEN length(CAST(id AS BLOB))<=?2 THEN id END FROM semantic_revision_producers WHERE revision_id=?1 ORDER BY id LIMIT 2",
                )?.query_map(params![selected.key, crate::navigation::RECORD_BYTES], |row| row.get::<_, Option<String>>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let mut ids = Vec::new();
                for producer in producers {
                    let producer = producer.context(SelectedIntegrity(
                        "selected semantic producer exceeds navigation budget".into(),
                    ))?;
                    selected_ensure!(
                        crate::graph_query::valid_producer_id(&producer),
                        "invalid selected semantic producer ID"
                    );
                    producer_bytes.push(serde_json::to_vec(&producer)?.len());
                    ids.push(producer);
                }
                multiple_producers = ids.len() > 1;
                let mut selector = selector.clone();
                selector.semantic_producer_policy = None;
                selector.semantic_producer_id = if ids.len() == 1 { ids.pop() } else { None };
                Some(crate::navigation::NavigationRequest::Source(selector))
            }
            _ => None,
        };
        let request = effective_request.as_ref().unwrap_or(request);
        // Navigation's source selector counts lines from the stored source BLOB,
        // and its member selector reads versioned class/node rows. Before either
        // consumes a selected document, authenticate it against that source BLOB
        // in this same read transaction. Never scan the entire workspace here.
        let selected_path: Option<String> = match request {
            crate::navigation::NavigationRequest::Source(s) => Some(s.path.clone()),
            crate::navigation::NavigationRequest::Member(s) => {
                // Locate the selected document by identity, not the
                // materialized kind.
                tx.query_row(
                    "SELECT m.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1 LIMIT 1",
                    params![id_codec::encode_sid(&s.class_id), selected.key],
                    |row| row.get(0),
                )
                .optional()?
            }
        };
        let mut selected_document = None;
        if let Some(path) = selected_path {
            // Gate allocation of the selected JSON/BLOB before decoding either.
            // This reads only SQLite byte lengths, not every workspace document.
            let sizes: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT length(d.source_bytes),length(d.source_bytes) FROM revision_documents m
                 JOIN document_versions d ON d.vid=m.vid
                 WHERE m.revision_id=?2 AND m.path=?1",
                    params![path, selected.key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((graph_len, native_len)) = sizes {
                if graph_len > 2 * 1024 * 1024 || native_len > 2 * 1024 * 1024 {
                    return Err(self.report_index_failure(
                        SelectedIntegrity(
                            "incompatible_index: selected navigation source exceeds budget".into(),
                        )
                        .into(),
                    ));
                }
                selected_document = Some(self.selected_graph_document_for(tx, &path, &selected)?);
            } else {
                let graph_file: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?2 AND path=?1)",
                    params![path, selected.key],
                    |row| row.get(0),
                )?;
                if graph_file {
                    return Err(self.report_index_failure(
                        SelectedIntegrity(
                            "incompatible_index: selected native document missing".into(),
                        )
                        .into(),
                    ));
                }
            }
        }
        crate::navigation::navigate(
            tx,
            request,
            revision,
            &selected.key,
            selected_document
                .as_ref()
                .map(|document| document.nodes.as_slice()),
            &mut |reader, selector| {
                if multiple_producers {
                    reader.warn("multiple semantic producers; showing syntax");
                }
                for bytes in &producer_bytes {
                    if !reader.charge(*bytes) {
                        return Ok(());
                    }
                }
                graph_read::navigation_calls(
                    self,
                    tx,
                    &selected,
                    selected_document.as_ref(),
                    selector,
                    reader,
                )
            },
        )
    }

    pub fn classes_at(
        &self,
        path: Option<&str>,
        query: &str,
        expected: Option<IndexPin>,
        offset: usize,
        limit: usize,
    ) -> Result<crate::class_diagram::ClassPage> {
        self.with_evidence(|tx| self.classes_in(tx, path, query, expected, (offset, limit), false))
    }
    fn classes_in(
        &self,
        tx: &Connection,
        path: Option<&str>,
        query: &str,
        expected: Option<IndexPin>,
        pagination: (usize, usize),
        pre_startup: bool,
    ) -> Result<crate::class_diagram::ClassPage> {
        use crate::class_diagram::ClassPage;
        let (offset, limit) = pagination;
        use crate::class_diagram::InvalidRequest;
        let path = path.filter(|path| !path.is_empty());
        ensure!(
            (1..=100).contains(&limit)
                && offset <= 1_000_000
                && query.len() <= 512
                && !query.contains('\0'),
            InvalidRequest("Class search allows 1–100 results and a query of at most 512 bytes.")
        );
        if let Some(path) = path {
            ensure!(
                !path.is_empty()
                    && path.len() <= 8192
                    && !path.contains(['\0', '\\', ':'])
                    && !path
                        .split('/')
                        .any(|part| part.is_empty() || part == "." || part == ".."),
                InvalidRequest("Choose a workspace-relative class source path.")
            );
        }
        let selected_revision = self.read_revision_for(tx, expected, pre_startup)?;
        let revision = selected_revision.pin;
        let (mut warnings, truncated) = class_metadata(tx, &selected_revision)?;
        let pattern = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut stmt = tx.prepare("SELECT c.id FROM classes c JOIN revision_documents m ON m.revision_id=?6 AND m.cid=c.cid WHERE (?1 IS NULL OR c.path=?1)
            AND (c.name LIKE ?2 ESCAPE '\\' OR c.qualified_name LIKE ?2 ESCAPE '\\' OR ('sid:v1:'||lower(hex(c.id)))=?3)
            ORDER BY CASE WHEN lower(c.name)=lower(?3) THEN 0 ELSE 1 END,c.qualified_name,c.path,c.id LIMIT ?4 OFFSET ?5")?;
        let ids = stmt
            .query_map(
                params![
                    path,
                    pattern,
                    query,
                    (limit + 1) as i64,
                    offset as i64,
                    selected_revision.key
                ],
                |r| stored_id_row(r, 0, false),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut items = Vec::new();
        let mut consumed = 0;
        let mut bytes = 0;
        let mut byte_limited = false;
        for id in ids.iter().take(limit) {
            let Some((class, size, clipped)) =
                presentation_class(self, tx, id, &selected_revision)?
            else {
                // Consume an individually oversized row so pagination always progresses.
                consumed += 1;
                byte_limited = true;
                continue;
            };
            if bytes + size > crate::class_diagram::PAGE_BYTES {
                byte_limited = true;
                break;
            }
            bytes += size;
            consumed += 1;
            byte_limited |= clipped;
            items.push(class);
        }
        let next_offset = (consumed < ids.len()).then_some(offset + consumed);
        if byte_limited {
            warnings.push(crate::class_diagram::BYTE_NOTICE.into());
        }
        let truncated = truncated || byte_limited;
        if let Some(path) = path
            && !path.ends_with(".java")
            && !path.ends_with(".py")
        {
            warnings
                .push("Class diagrams currently support Java and Python declarations only.".into());
        }
        Ok(ClassPage {
            revision,
            items,
            next_offset,
            truncated,
            warnings,
            require_index: false,
        })
    }

    /// Bounded one-hop relation reads and seed resolution share a revision-pinned
    /// read transaction. No filesystem access or graph/provider augmentation.
    pub fn class_diagram_at(
        &self,
        request: &crate::class_diagram::ClassDiagramRequest,
    ) -> Result<crate::class_diagram::ClassDiagram> {
        request.validate()?;
        self.with_evidence(|tx| self.class_diagram_in(tx, request, false))
    }
    fn class_diagram_in(
        &self,
        tx: &Connection,
        request: &crate::class_diagram::ClassDiagramRequest,
        pre_startup: bool,
    ) -> Result<crate::class_diagram::ClassDiagram> {
        use crate::class_diagram::{self, InvalidRequest};
        let selected = self.read_revision_for(tx, Some(request.expected_revision), pre_startup)?;
        let revision = selected.pin;
        let (warnings, truncated) = class_metadata(tx, &selected)?;
        let (seed, clipped) = resolve_class(self, tx, &request.seed, &selected)?;
        // Explicitly selected measured declarations are independent roots, never
        // connected by lexical type-name matches or candidate relationships.
        let mut seeds = vec![seed.symbol.id.clone()];
        let mut classes = BTreeMap::from([(seed.symbol.id.clone(), seed)]);
        for expanded in &request.expanded {
            let (class, _) = resolve_class(self, tx, expanded, &selected)?;
            if !classes.contains_key(&class.symbol.id) {
                ensure!(
                    classes.len() < class_diagram::MAX_NODES,
                    InvalidRequest("Too many selected classes.")
                );
                seeds.push(class.symbol.id.clone());
                classes.insert(class.symbol.id.clone(), class);
            }
        }
        class_diagram::project(
            revision,
            &seeds,
            &classes,
            vec![],
            vec![],
            warnings,
            truncated || clipped,
        )
    }

    pub fn symbols(&self, query: &str, limit: usize) -> Result<Vec<Symbol>> {
        Ok(self.symbols_at(query, limit)?.1)
    }
    pub fn symbols_at(&self, query: &str, limit: usize) -> Result<(IndexPin, Vec<Symbol>)> {
        ensure!(query.len() <= 8192, "search query too long");
        self.with_evidence(|tx| self.symbols_in(tx, query, limit, false))
    }
    fn symbols_in(
        &self,
        tx: &Connection,
        query: &str,
        limit: usize,
        pre_startup: bool,
    ) -> Result<(IndexPin, Vec<Symbol>)> {
        let selected = self.read_revision_for(tx, None, pre_startup)?;
        let revision = selected.pin;
        let rows = symbol_trigrams::graph_hits(tx, query, limit.min(150) as i64)?;
        let mut documents = BTreeMap::new();
        for (path, _) in &rows {
            if !documents.contains_key(path) {
                documents.insert(
                    path.clone(),
                    self.selected_graph_document_for(tx, path, &selected)?,
                );
            }
        }
        let values = rows
            .iter()
            .map(|(path, id)| {
                documents[path]
                    .nodes
                    .iter()
                    .find(|node| node.id == *id)
                    .cloned()
                    .ok_or_else(|| {
                        anyhow::Error::from(SelectedIntegrity(
                            "incompatible_index: selected search symbol missing".into(),
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok((revision, values))
    }

    pub fn symbol(&self, id: &str) -> Result<Option<Symbol>> {
        Ok(self.symbol_at(id, None)?.map(|(_, v)| v))
    }
    pub fn source(&self, path: &str) -> Result<Option<SourceFile>> {
        Ok(self.source_at(path, None)?.map(|(_, v)| v))
    }
    pub fn symbol_at(
        &self,
        id: &str,
        expected_revision: Option<IndexPin>,
    ) -> Result<Option<(IndexPin, Symbol)>> {
        self.with_evidence(|tx| self.symbol_in(tx, id, expected_revision, false))
    }
    fn symbol_in(
        &self,
        tx: &Connection,
        id: &str,
        expected_revision: Option<IndexPin>,
        pre_startup: bool,
    ) -> Result<Option<(IndexPin, Symbol)>> {
        let selected = self.read_revision_for(tx, expected_revision, pre_startup)?;
        let revision = selected.pin;
        let selected_path: Option<String> = tx
                .query_row("SELECT m.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1", params![id_codec::encode_sid(id), selected.key], |r| r.get(0))
                .optional()?;
        let node = if let Some(path) = selected_path {
            self.selected_graph_document_for(tx, &path, &selected)?
                .nodes
                .into_iter()
                .find(|node| node.id == id)
        } else {
            None
        };
        Ok(node.map(|node| (revision, node)))
    }

    #[cfg(feature = "test-support")]
    /// Alter an already validated selected BLOB only in the live test store.
    /// This intentionally never publishes or reopens the malformed source.
    pub fn mcp_replace_validated_source_bytes_for_tests(
        &self,
        writer: &topology::WriterGuard,
        path: &str,
        bytes: &[u8],
    ) -> Result<()> {
        use sha2::{Digest, Sha256};
        writer.belongs_to(&self.identity)?;
        self.identity.verify()?;
        writer.verify()?;
        let response = self.evidence_response()?;
        ensure!(
            response.source_at(path, None)?.is_some(),
            "fixture source absent"
        );
        response.finish(())?;
        drop(response);
        let db = self.cache_write()?;
        let changed = storage_result(db.execute(
            "UPDATE document_versions SET source_bytes=?1,byte_length=?2,content_hash=?3
             WHERE vid=(SELECT vid FROM revision_documents
                 WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision
                     FROM index_metadata WHERE singleton=1) AND path=?4)",
            params![
                bytes,
                bytes.len() as i64,
                hex::encode(Sha256::digest(bytes)),
                path
            ],
        ))?;
        ensure!(changed == 1, "fixture selected source absent");
        Ok(())
    }

    /// Read only the selected raw source identity, without the native witness or UTF-8
    /// conversion used by the public source reader. W5 verifies the entire cached BLOB.
    fn mcp_selected_raw_source_for(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
    ) -> Result<Option<(String, Vec<u8>)>> {
        let mut statement = db.prepare(
            "SELECT v.id,r.source_set_id,m.language FROM revision_documents m
             JOIN native_revisions r ON r.id=m.revision_id
             JOIN document_versions v ON v.vid=m.vid
             WHERE m.revision_id=?1 AND m.path=?2 LIMIT 2",
        )?;
        let mut rows = statement.query(params![selected.key, path])?;
        let manifest = rows
            .next()?
            .map(|row| -> rusqlite::Result<_> {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .transpose()?;
        ensure!(
            rows.next()?.is_none(),
            SelectedIntegrity("store_unavailable: ambiguous selected source path".into())
        );
        let Some((version, source_set, language)) = manifest else {
            let declared: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?1 AND path=?2)",
                params![selected.key, path],
                |row| row.get(0),
            )?;
            ensure!(
                !declared,
                SelectedIntegrity("store_unavailable: selected source version absent".into())
            );
            return Ok(None);
        };
        let row: Option<(String, String, String, String, i64, Vec<u8>)> = db
            .query_row(
                "SELECT r.source_set_id,v.language,v.path,v.content_hash,v.byte_length,v.source_bytes
                 FROM document_versions v JOIN native_revisions r ON r.id=?2
                 WHERE v.id=?1",
                params![version, selected.key],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;
        let (stored_set, stored_language, stored_path, hash, length, bytes) =
            row.ok_or_else(|| {
                SelectedIntegrity("store_unavailable: selected source version absent".into())
            })?;
        use sha2::{Digest, Sha256};
        ensure!(
            stored_set == source_set && stored_language == language && stored_path == path,
            SelectedIntegrity("store_unavailable: selected raw source identity mismatch".into())
        );
        // Stored bytes that disagree with their own recorded length or hash are
        // corrupt: schedule a rebuild. This never covers decoding, so invalid
        // UTF-8 outside a requested range can't trigger it (W5).
        if usize::try_from(length).ok() != Some(bytes.len())
            || hash != hex::encode(Sha256::digest(&bytes))
        {
            return Err(self.report_index_failure(
                SelectedIntegrity(
                    "store_unavailable: selected raw source length or digest mismatch".into(),
                )
                .into(),
            ));
        }
        Ok(Some((hash, bytes)))
    }

    fn selected_source_row_for(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
    ) -> Result<Option<SourceFile>> {
        Self::selected_source_row_bounded_for(db, path, 256 * 1024 * 1024, selected)
    }
    #[cfg(test)]
    fn selected_source_row_bounded(
        db: &Connection,
        path: &str,
        max_bytes: i64,
    ) -> Result<Option<SourceFile>> {
        let selected = ReadRevision::current(db)?;
        Self::selected_source_row_bounded_for(db, path, max_bytes, &selected)
    }
    fn selected_source_row_bounded_for(
        db: &Connection,
        path: &str,
        max_bytes: i64,
        selected: &ReadRevision,
    ) -> Result<Option<SourceFile>> {
        Self::selected_source_row_bounded_for_inner(db, path, max_bytes, selected, None)
    }
    fn selected_source_row_bounded_for_inner(
        db: &Connection,
        path: &str,
        max_bytes: i64,
        selected: &ReadRevision,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<Option<SourceFile>> {
        #[cfg(test)]
        count_selected_read(|counts| counts.source_reads += 1);
        optional_hint_check(progress)?;
        let selected_sql = "FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id
            JOIN document_versions d ON d.vid=m.vid
              AND d.language=m.language AND d.path=m.path
            WHERE m.revision_id=?2 AND m.path=?1";
        let sql = format!(
            "SELECT length(d.source_bytes),length(CAST(r.source_set_id AS BLOB)),length(CAST(d.language AS BLOB)),length(CAST(d.path AS BLOB)),length(CAST(d.content_hash AS BLOB)) {selected_sql} LIMIT 2"
        );
        let mut stmt = db.prepare_cached(&sql)?;
        let mut sizes = stmt.query(params![path, selected.key])?;
        let mut present = false;
        while let Some(row) = sizes.next()? {
            optional_hint_check(progress)?;
            selected_ensure!(!present, "incompatible_index: ambiguous manifest source");
            present = true;
            let bytes: i64 = row.get(0)?;
            selected_ensure!(
                bytes >= 0
                    && bytes <= max_bytes
                    && (1usize..5).all(|i| -> bool {
                        row.get::<_, i64>(i)
                            .is_ok_and(|length| (0..=16 * 1024).contains(&length))
                    }),
                "incompatible_index: selected source byte budget exceeded"
            );
        }
        drop(sizes);
        drop(stmt);
        if !present {
            let declared: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?1 AND path=?2)",
                params![selected.key, path],
                |row| row.get(0),
            )?;
            ensure!(
                !declared,
                SelectedIntegrity("incompatible_index: selected source version absent".into())
            );
            return Ok(None);
        }
        let sql = format!(
            "SELECT r.source_set_id,d.language,d.path,d.content_hash,d.byte_length,d.source_bytes {selected_sql}"
        );
        let checked: Result<(String, String, String, String, Vec<u8>)> =
            db.prepare_cached(&sql)?
                .query_row(params![path, selected.key], |r| {
                    Ok((|| -> Result<_> {
                        use sha2::{Digest, Sha256};
                        let source_set: String = r.get(0)?;
                        let language: String = r.get(1)?;
                        let stored_path: String = r.get(2)?;
                        let hash: String = r.get(3)?;
                        let length: i64 = r.get(4)?;
                        let raw = r.get_ref(5)?.as_blob().context(SelectedIntegrity(
                            "incompatible_index: stored source is not a BLOB".into(),
                        ))?;
                        let mut digest = Sha256::new();
                        for chunk in raw.chunks(64 * 1024) {
                            optional_hint_check(progress)?;
                            digest.update(chunk);
                        }
                        optional_hint_check(progress)?;
                        selected_ensure!(
                            source_set.starts_with("source-set:v1:")
                                && stored_path == path
                                && length == raw.len() as i64
                                && hash == hex::encode(digest.finalize()),
                            "incompatible_index: source hash mismatch"
                        );
                        Ok((source_set, language, stored_path, hash, raw.to_vec()))
                    })())
                })?;
        let (_source_set, language, stored_path, hash, bytes) = checked?;
        optional_hint_check(progress)?;
        let file = SourceFile {
            path: stored_path,
            hash,
            language,
            text: String::from_utf8(bytes).context(SelectedIntegrity(
                "incompatible_index: stored source encoding".into(),
            ))?,
        };
        let mut budget = EncodedSourceBudget {
            bytes: 0,
            max_bytes: usize::try_from(max_bytes)?.saturating_add(16 * 1024),
        };
        serde_json::to_writer(&mut budget, &file)?;
        optional_hint_check(progress)?;
        Ok(Some(file))
    }
    pub fn source_at(
        &self,
        path: &str,
        expected_revision: Option<IndexPin>,
    ) -> Result<Option<(IndexPin, SourceFile)>> {
        self.with_evidence(|tx| {
            let selected = self.read_revision(tx, expected_revision)?;
            let source = self.selected_source_row_for(tx, path, &selected)?;
            Ok(source.map(|file| (selected.pin, file)))
        })
    }
    /// Catalog reads pin revision and rows to one SQLite read transaction.
    /// Enrich only the visible tree page from one cached index snapshot.
    pub fn tree_metadata(
        &self,
        root: &Path,
        items: &mut [crate::file_tree::Entry],
    ) -> Result<(IndexPin, String)> {
        self.tree_metadata_with_finish_hook(root, items, || Ok(()))
    }
    fn tree_metadata_with_finish_hook(
        &self,
        root: &Path,
        items: &mut [crate::file_tree::Entry],
        before_finish: impl FnOnce() -> Result<()>,
    ) -> Result<(IndexPin, String)> {
        let (revision, workspace_root, overlays) = self.with_evidence_hook(|tx| {
            let revision = self.read_status(tx)?.revision;
            let workspace = Path::new(&self.workspace_root);
            let mut count_stmt = tx.prepare(
                "SELECT (SELECT count(*) FROM graph_nodes n WHERE n.gid=m.gid AND n.kind IN ('function','method')) FROM revision_documents m JOIN index_metadata active_manifest ON active_manifest.singleton=1 AND m.revision_id='pin:v1:'||active_manifest.index_generation||':'||active_manifest.index_revision WHERE m.path=?1",
            )?;
            let mut overlays = Vec::new();
            for (index, item) in items.iter().enumerate().filter(|(_, entry)| entry.kind == "file") {
                let absolute = root.join(&item.path);
                let Ok(relative) = absolute.strip_prefix(workspace) else { continue };
                let Some(relative) = relative.to_str() else { continue };
                let count: Option<i64> = count_stmt
                    .query_row([relative], |row| row.get(0))
                    .optional()?;
                let Some(count) = count else { continue };
                overlays.push((index, relative.to_owned(), usize::try_from(count)?));
            }
            Ok((revision, self.workspace_root.clone(), overlays))
        }, before_finish)?;
        for (index, path, count) in overlays {
            items[index].indexed_path = Some(path);
            items[index].method_count = Some(count);
        }
        Ok((revision, workspace_root))
    }
    fn tree_metadata_in(
        &self,
        tx: &Connection,
        root: &Path,
        items: &mut [crate::file_tree::Entry],
        pre_startup: bool,
    ) -> Result<(IndexPin, String, TreeOverlays)> {
        let revision = self.read_status_for(tx, pre_startup)?.revision;
        let workspace = Path::new(&self.workspace_root);
        let mut count_stmt = tx.prepare(
                "SELECT (SELECT count(*) FROM graph_nodes n WHERE n.gid=m.gid AND n.kind IN ('function','method')) FROM revision_documents m JOIN index_metadata active_manifest ON active_manifest.singleton=1 AND m.revision_id='pin:v1:'||active_manifest.index_generation||':'||active_manifest.index_revision WHERE m.path=?1",
            )?;
        let mut overlays = Vec::new();
        for (index, item) in items
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.kind == "file")
        {
            let absolute = root.join(&item.path);
            let Ok(relative) = absolute.strip_prefix(workspace) else {
                continue;
            };
            let Some(relative) = relative.to_str() else {
                continue;
            };
            let count: Option<i64> = count_stmt
                .query_row([relative], |row| row.get(0))
                .optional()?;
            let Some(count) = count else { continue };
            overlays.push((index, relative.to_owned(), usize::try_from(count)?));
        }
        Ok((revision, self.workspace_root.clone(), overlays))
    }
    pub fn files_at(
        &self,
        expected: Option<IndexPin>,
        offset: usize,
        limit: usize,
    ) -> Result<serde_json::Value> {
        ensure!(
            (1..=200).contains(&limit) && offset <= i64::MAX as usize,
            "invalid catalog pagination"
        );
        self.with_evidence_response(
            |response| {
                response
                    .store
                    .files_in(&response.db, expected, offset, limit, false)
            },
            || Ok(()),
            |_| {},
        )
    }
    fn files_in(
        &self,
        tx: &Connection,
        expected: Option<IndexPin>,
        offset: usize,
        limit: usize,
        pre_startup: bool,
    ) -> Result<serde_json::Value> {
        let selected = self.read_revision_for(tx, expected, pre_startup)?;
        let revision = selected.pin;
        let mut stmt = tx.prepare(
            "SELECT m.path,d.language,g.gid FROM revision_documents m
                 LEFT JOIN document_versions d ON d.vid=m.vid
                   AND d.language=m.language AND d.path=m.path
                 LEFT JOIN graph_projections g ON g.gid=m.gid AND g.vid=m.vid
                 WHERE m.revision_id=?3 ORDER BY m.path LIMIT ?1 OFFSET ?2",
        )?;
        let source_rows: Vec<(String, Option<String>, Option<i64>)> = stmt
            .query_map(
                params![(limit + 1) as i64, offset as i64, selected.key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;
        // The extra row only decides whether a next page exists; it isn't
        // returned, so only the returned rows are checked and counted.
        let next = (source_rows.len() > limit).then_some(offset + limit);
        let mut items = Vec::new();
        for (path, language, projection_id) in source_rows.into_iter().take(limit) {
            // A manifest row whose version or graph projection is absent is
            // corruption, not an empty file.
            let (Some(language), Some(projection_id)) = (language, projection_id) else {
                return Err(self.report_index_failure(
                    SelectedIntegrity(
                        "incompatible_index: selected document projection absent".into(),
                    )
                    .into(),
                ));
            };
            let methods: i64 = tx.query_row(
                "SELECT count(*) FROM graph_nodes WHERE gid=?1
                    AND kind IN ('function','method')",
                [&projection_id],
                |r| r.get(0),
            )?;
            items.push(serde_json::json!({"path":path,"language":language,"methodCount":methods}));
        }
        Ok(serde_json::json!({"revision":revision,"items":items,"nextOffset":next}))
    }

    pub fn methods_at(
        &self,
        path: &str,
        expected: Option<IndexPin>,
    ) -> Result<Option<serde_json::Value>> {
        self.with_evidence(|tx| self.methods_in(tx, path, expected, false))
    }
    fn methods_in(
        &self,
        tx: &Connection,
        path: &str,
        expected: Option<IndexPin>,
        pre_startup: bool,
    ) -> Result<Option<serde_json::Value>> {
        let selected = self.read_revision_for(tx, expected, pre_startup)?;
        let revision = selected.pin;
        let projection_id: Option<i64> = tx
            .query_row(
                "SELECT m.gid FROM revision_documents m
                     WHERE m.revision_id=?2 AND m.path=?1",
                params![path, selected.key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(_projection_id) = projection_id else {
            return Ok(None);
        };
        let mut symbols = self.selected_graph_document_for(tx, path, &selected)?.nodes;
        symbols.retain(|node| matches!(node.kind, SymbolKind::Method | SymbolKind::Function));
        symbols.sort_by(|a, b| (a.range.start_byte, &a.id).cmp(&(b.range.start_byte, &b.id)));
        let mut items = Vec::new();
        for symbol in symbols.into_iter().take(1001) {
            items.push(serde_json::json!({"symbol":symbol,"consequential":true,
                    "reason":"Conservative heuristic: retained; triviality is not proven"}));
        }
        let truncated = items.len() > 1000;
        items.truncate(1000);
        Ok(Some(
            serde_json::json!({"revision":revision,"items":items,"truncated":truncated}),
        ))
    }

    /// All measured declarations in one cached file, retaining their exact native IDs.
    pub fn file_symbols_at(
        &self,
        path: &str,
        expected: IndexPin,
        offset: usize,
        limit: usize,
    ) -> Result<Option<crate::files_view::Outline>> {
        self.with_evidence(|tx| self.file_symbols_in(tx, path, expected, offset, limit, false))
    }
    fn file_symbols_in(
        &self,
        tx: &Connection,
        path: &str,
        expected: IndexPin,
        offset: usize,
        limit: usize,
        pre_startup: bool,
    ) -> Result<Option<crate::files_view::Outline>> {
        use crate::files_view::{InvalidRequest, Outline, OutlineItem, RESPONSE_LIMIT};
        ensure!(
            crate::files_view::valid_path(path)
                && (1..=200).contains(&limit)
                && offset <= i64::MAX as usize,
            InvalidRequest
        );
        let selected = self.read_revision_for(tx, Some(expected), pre_startup)?;
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?1 AND path=?2)",
            params![selected.key, path],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        let header = self.fetch_selected_header(tx, path, &selected, None)?;
        let fetched = self.fetch_selected_rows(tx, header, &selected, None)?;
        let mut metadata: BTreeMap<_, _> = fetched
            .declarations
            .iter()
            .map(|d| (d.syntax_id.clone(), (d.kind.clone(), d.header.clone())))
            .collect();
        let mut nodes = Self::project_fetched(fetched)?.nodes;
        nodes.sort_by(|a, b| (a.range.start_byte, &a.id).cmp(&(b.range.start_byte, &b.id)));
        let count = nodes.len();
        let mut result = Outline {
            revision: selected.pin,
            path: path.into(),
            items: vec![],
            next_offset: None,
            truncated: false,
            warnings: vec![],
        };
        for symbol in nodes.into_iter().skip(offset).take(limit) {
            let (native_kind, header) = metadata.remove(&symbol.id).context(SelectedIntegrity(
                "incompatible_index: outline native declaration missing".into(),
            ))?;
            result.items.push(OutlineItem {
                symbol,
                native_kind,
                header,
            });
            let next = offset + result.items.len();
            result.next_offset = (next < count).then_some(next);
            result.truncated = result.next_offset.is_some();
            if serde_json::to_vec(&result)?.len() > RESPONSE_LIMIT {
                result.items.pop();
                ensure!(
                    !result.items.is_empty(),
                    "response_too_large: Files outline item"
                );
                result.next_offset = Some(offset + result.items.len());
                result.truncated = true;
                break;
            }
        }
        ensure!(
            serde_json::to_vec(&result)?.len() <= RESPONSE_LIMIT,
            "response_too_large: Files outline"
        );
        Ok(Some(result))
    }
    pub fn syntax_matches_at(
        &self,
        request: &crate::files_view::SyntaxMatchesRequest,
    ) -> Result<Option<crate::files_view::SyntaxMatches>> {
        request.validate()?;
        self.with_evidence(|tx| self.syntax_matches_in(tx, request, false))
    }
    fn syntax_matches_in(
        &self,
        tx: &Connection,
        request: &crate::files_view::SyntaxMatchesRequest,
        pre_startup: bool,
    ) -> Result<Option<crate::files_view::SyntaxMatches>> {
        use crate::files_view::{self, MatchScope, SyntaxMatches};
        request.validate()?;
        let selected = self.read_revision_for(tx, Some(request.expected_revision), pre_startup)?;
        let Some(scope) = v8_owner_scope_for(tx, &request.symbol_id, &selected)? else {
            return Ok(None);
        };
        // Decode the selected document's native rows strictly, including noncallables.
        let declaration = read_native_declarations_v8(tx, &scope, None, true)?
            .into_iter()
            .find(|d| d.syntax_id == request.symbol_id)
            .context(SelectedIntegrity(
                "incompatible_index: match declaration missing".into(),
            ))?;
        let mut result = SyntaxMatches::new(
            request,
            MatchScope {
                source_set_id: scope.key.source_set_id.clone(),
                language: scope.key.language.clone(),
                spelling: declaration.name.clone(),
            },
        );
        if declaration.name_range.is_none()
            || declaration.name.as_ref().is_none_or(|n| n.is_empty())
        {
            result.scope.spelling = None;
            result.warn("Selected declaration has no captured identifier name; syntax matching unavailable.");
            result.bounded()?;
            return Ok(Some(result));
        }
        if let Some(cursor) = &request.continuation {
            let present: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_documents m
                 JOIN native_revisions r ON r.id=m.revision_id WHERE m.revision_id=?1
                 AND m.path=?2 AND m.language=?3 AND r.source_set_id=?4)",
                params![
                    selected.key,
                    cursor.path,
                    scope.key.language,
                    scope.key.source_set_id
                ],
                |row| row.get(0),
            )?;
            ensure!(present, files_view::InvalidRequest);
        }
        let started = Instant::now();
        let mut bytes = 0usize;
        let mut after: Option<String> = None;
        // Read manifest entries one at a time. Never materialize a checkout graph/catalog.
        let mut entries = tx.prepare(
            "SELECT m.path,length(d.source_bytes),d.byte_length FROM revision_documents m
             JOIN native_revisions r ON r.id=m.revision_id
             LEFT JOIN document_versions d ON d.vid=m.vid AND d.language=m.language AND d.path=m.path
             WHERE m.revision_id=?1 AND m.language=?2 AND (?3 IS NULL OR m.path>=?3)
             AND (?4 IS NULL OR m.path>?4) AND r.source_set_id=?5 ORDER BY m.path LIMIT 1")?;
        loop {
            let entry: Option<(String, Option<i64>, Option<i64>)> = entries
                .query_row(
                    params![
                        selected.key,
                        scope.key.language,
                        request.continuation.as_ref().map(|c| c.path.as_str()),
                        after,
                        scope.key.source_set_id
                    ],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((path, size, recorded)) = entry else {
                break;
            };
            let (Some(size), Some(recorded)) = (size, recorded) else {
                return Err(
                    SelectedIntegrity("incompatible_index: match source absent".into()).into(),
                );
            };
            selected_ensure!(
                size >= 0 && size == recorded,
                "incompatible_index: match source length invalid"
            );
            let start = request
                .continuation
                .as_ref()
                .filter(|c| c.path == path)
                .map_or(0, |c| c.offset);
            ensure!(start <= size as usize, files_view::InvalidRequest);
            if started.elapsed() >= files_view::TOTAL_TIME_LIMIT {
                result.warnings.push(
                    "Request syntax scan budget reached; continue for remaining cached files."
                        .into(),
                );
                result.resume(&path, start);
                break;
            }
            if size as usize > files_view::FILE_INPUT_LIMIT {
                ensure!(start == 0, files_view::InvalidRequest);
                result.warn("Cached file exceeds 2 MiB syntax scan input; file skipped and coverage incomplete.");
                after = Some(path);
                continue;
            }
            if bytes.saturating_add(size as usize) > files_view::TOTAL_INPUT_LIMIT {
                result.warnings.push(
                    "Request syntax scan budget reached; continue for remaining cached files."
                        .into(),
                );
                result.resume(&path, start);
                break;
            }
            bytes += size as usize;
            let file = self
                .selected_source_row_for(tx, &path, &selected)?
                .context(SelectedIntegrity(
                    "incompatible_index: match source missing".into(),
                ))?;
            ensure!(
                file.text.is_char_boundary(start),
                files_view::InvalidRequest
            );
            let excluded = (path == scope.key.path)
                .then_some(declaration.name_range.as_ref())
                .flatten()
                .map(|r| (r.start, r.end));
            if files_view::scan_file(&mut result, request, &file, start, excluded, started)? {
                break;
            }
            after = Some(path);
        }
        result.bounded()?;
        Ok(Some(result))
    }

    /// Only cached source and measured calls from the same snapshot are used.
    pub fn sequence_at(
        &self,
        seed: &str,
        expected: IndexPin,
        show_all: bool,
    ) -> Result<Option<crate::behavior::SequenceView>> {
        self.with_evidence(|tx| self.sequence_in(tx, seed, expected, show_all, false))
    }
    fn sequence_in(
        &self,
        tx: &Connection,
        seed: &str,
        expected: IndexPin,
        show_all: bool,
        pre_startup: bool,
    ) -> Result<Option<crate::behavior::SequenceView>> {
        let selected_revision = self.read_revision_for(tx, Some(expected), pre_startup)?;
        let revision = selected_revision.pin;
        let path: Option<String> = tx
            .query_row(
                "SELECT m.path FROM graph_nodes n JOIN revision_documents m
             ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1",
                params![id_codec::encode_sid(seed), selected_revision.key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(path) = path else { return Ok(None) };
        let document = self.selected_graph_document_for(tx, &path, &selected_revision)?;
        let symbol = document
            .nodes
            .iter()
            .find(|node| node.id == seed)
            .ok_or_else(|| {
                SelectedIntegrity("incompatible_index: selected sequence seed missing".into())
            })?;
        ensure!(
            matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method),
            "invalid sequence symbol kind"
        );
        let mut calls = document.graph_calls;
        calls.sort_by(|a, b| (a.range.start_byte, &a.id).cmp(&(b.range.start_byte, &b.id)));
        let file = document.file;
        let mut view = crate::behavior::build_sequence(revision, symbol, &file, &calls, show_all)?;
        graph_read::sequence_calls(self, tx, &selected_revision, &mut view)?;
        Ok(Some(view))
    }

    fn read_graph_for(&self, db: &Connection, selected: &ReadRevision) -> Result<Graph> {
        let (stats_type, stats_len, diagnostics_type, diagnostics_len): (String, i64, String, i64) =
            db.query_row(
                "SELECT typeof(graph_stats),length(CAST(graph_stats AS BLOB)),
                typeof(graph_diagnostics),length(CAST(graph_diagnostics AS BLOB))
             FROM native_revisions WHERE id=?1",
                [&selected.key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        selected_ensure!(
            stats_type == "text"
                && diagnostics_type == "text"
                && (0..=GRAPH_FIELD_MAX_BYTES).contains(&stats_len)
                && (0..=GRAPH_FIELD_MAX_BYTES).contains(&diagnostics_len)
                && stats_len + diagnostics_len <= GRAPH_PAIR_MAX_BYTES,
            "incompatible_index: retained graph metadata byte budget exceeded"
        );
        let (raw_stats, raw_diagnostics): (String, String) = db.query_row(
            "SELECT graph_stats,graph_diagnostics FROM native_revisions WHERE id=?1",
            [&selected.key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let stats: IndexStats = serde_json::from_str(&raw_stats).context(SelectedIntegrity(
            "incompatible_index: stored JSON decode failed".into(),
        ))?;
        let diagnostics: Vec<Diagnostic> = serde_json::from_str(&raw_diagnostics).context(
            SelectedIntegrity("incompatible_index: stored JSON decode failed".into()),
        )?;
        let paths: Vec<String> = db
            .prepare(
                "SELECT m.path FROM revision_documents m WHERE m.revision_id=?1 ORDER BY m.path",
            )?
            .query_map([&selected.key], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        let mut files = Vec::new();
        let mut nodes = Vec::new();
        let mut calls = Vec::new();
        let mut regions = Vec::new();
        for path in &paths {
            let document = self.selected_graph_document_for(db, path, selected)?;
            files.push(document.file);
            nodes.extend(document.nodes);
            calls.extend(document.graph_calls);
            regions.extend(document.graph_regions);
        }
        nodes.sort_by(|a, b| a.id.cmp(&b.id));
        calls.sort_by(crate::indexer::compare_reader_calls);
        regions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(Graph {
            schema_version: SCHEMA_VERSION,
            files,
            nodes,
            calls,
            regions,
            stats,
            diagnostics,
        })
    }
    pub fn graph(&self) -> Result<Graph> {
        self.graph_at(None)
    }
    pub fn graph_at(&self, expected: Option<IndexPin>) -> Result<Graph> {
        self.with_evidence(|tx| {
            let selected = self.read_revision(tx, expected)?;
            self.read_graph_for(tx, &selected)
        })
    }
    /// Inspect retention state through the protected index read path in integration tests.
    #[cfg(feature = "test-support")]
    pub fn retention_state_for_tests(&self, revision_id: &str) -> Result<RetentionStateForTests> {
        self.with_evidence(|db| {
            let revision = storage_result(db.query_row(
                "SELECT s.state,
                        (SELECT count(*) FROM native_revision_release_debt d
                         WHERE d.revision_id=s.revision_id),
                        (SELECT count(*) FROM revision_documents r
                         WHERE r.revision_id=s.revision_id)
                 FROM native_revision_supersessions s WHERE s.revision_id=?1",
                [revision_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ))?;
            let pending = storage_result(db.query_row(
                "SELECT (SELECT count(*) FROM native_revision_supersessions WHERE state='pending'),
                        (SELECT count(*) FROM native_revision_release_debt)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ))?;
            Ok((revision, pending))
        })
    }

    pub fn query_view(&self, query: &ViewQuery) -> Result<Option<ViewResult>> {
        self.query_view_at(query, None)
    }

    pub fn query_view_at(
        &self,
        query: &ViewQuery,
        expected_pin: Option<&IndexPin>,
    ) -> Result<Option<ViewResult>> {
        query.validate()?;
        self.with_evidence(|tx| self.query_view_in(tx, query, expected_pin))
    }
    fn query_view_in(
        &self,
        tx: &Connection,
        query: &ViewQuery,
        expected_pin: Option<&IndexPin>,
    ) -> Result<Option<ViewResult>> {
        self.query_view_in_for(tx, query, expected_pin, false)
    }
    fn query_view_in_for(
        &self,
        tx: &Connection,
        query: &ViewQuery,
        expected_pin: Option<&IndexPin>,
        pre_startup: bool,
    ) -> Result<Option<ViewResult>> {
        let selected = self.read_revision_for(tx, expected_pin.copied(), pre_startup)?;
        let revision = selected.pin;
        let selected_path: Option<String> = tx
            .query_row("SELECT m.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.gid=n.gid WHERE n.id=?1", params![id_codec::encode_sid(&query.seed), selected.key], |r| {
                r.get(0)
            })
            .optional()?;
        let Some(path) = selected_path else {
            return Ok(None);
        };
        let seed_document = self.selected_graph_document_for(tx, &path, &selected)?;
        let seed = seed_document
            .nodes
            .iter()
            .find(|symbol| symbol.id == query.seed)
            .cloned()
            .ok_or_else(|| {
                SelectedIntegrity("incompatible_index: selected view seed missing".into())
            })?;
        let mut calls = Vec::new();
        let mut region_ids = BTreeSet::new();
        let mut truncated = false;
        if !query.exclude_paths.iter().any(|p| seed.path.starts_with(p)) {
            // A call's owner belongs to the seed's document. Sort its calls,
            // then take the SQL LIMIT before filtering excluded paths.
            let mut ordered: Vec<CallSite> = seed_document
                .graph_calls
                .iter()
                .filter(|call| call.caller == seed.id)
                .cloned()
                .collect();
            ordered.sort_by(crate::indexer::compare_reader_calls);
            for call in ordered.into_iter().take(query.max_calls + 1) {
                if query.exclude_paths.iter().any(|p| call.path.starts_with(p)) {
                    continue;
                }
                if calls.len() >= query.max_calls {
                    truncated = true;
                    break;
                }
                region_ids.extend(call.regions.iter().cloned());
                calls.push(call);
            }
        }
        let mut regions = BTreeMap::new();
        while let Some(id) = region_ids.pop_first() {
            let region = seed_document
                .graph_regions
                .iter()
                .find(|region| region.id == id)
                .cloned();
            if let Some(region) = region {
                if let Some(parent) = &region.parent {
                    region_ids.insert(parent.clone());
                }
                regions.insert(id, region);
            }
        }
        Ok(Some(ViewResult {
            revision,
            query: query.clone(),
            nodes: vec![seed],
            calls,
            regions: regions.into_values().collect(),
            truncated,
            omitted_nodes: 0,
            warnings: if truncated {
                vec!["Measured calls truncated at the request limit.".into()]
            } else {
                vec![]
            },
        }))
    }
}

// The SQL runs against encoded BLOB keys. Its budget must still match the old
// TEXT-key representation, including virtual class and call key components.
#[cfg(test)]
#[test]
fn selected_ancillary_budget_matches_main_text_key_bytes() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V29).unwrap();
    // The aggregate needs only synthetic child rows; FK behavior is covered by
    // publication/retention tests, not this isolated budget-SQL fixture.
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let version = "version:é";
    let source_set = "source-set:v1:root-é";
    let graph_hash = "a".repeat(64);
    let class_hash = "b".repeat(64);
    let owner = [0x11; 16];
    let target = [0x22; 16];
    let call = [0x33; 16];
    let region = [0x44; 16];
    db.execute(
        "INSERT INTO native_version_coverage_roles(vid,role_kind,role,ordinal)
         VALUES(1,'supported','definition',0)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO native_version_declarations(vid,syntax_id,kind,role,name,lookup_key,key_signature_present,
         key_type_parameter_count,key_variadic,key_ordinal,start_byte,end_byte,
         name_start,name_end)
         VALUES(1,?1,'function','production','héllo','hello',1,0,0,0,0,9,0,5)",
        [&owner[..]],
    ).unwrap();
    db.execute(
        "INSERT INTO native_version_calls(vid,id,owner_syntax_id,ordinal,start_byte,end_byte,language,callee_lookup_key)
         VALUES(1,?1,?2,0,0,7,'javascript','hello')",
        params![&call[..], &owner[..]],
    ).unwrap();
    db.execute(
        "INSERT INTO native_version_control_regions(vid,id,owner_syntax_id,ordinal,kind,start_byte,end_byte,arm)
         VALUES(1,?1,?2,0,'if',0,7,'then')",
        params![&region[..], &owner[..]],
    ).unwrap();
    db.execute(
        "INSERT INTO native_version_call_regions(vid,call_id,region_id,ordinal) VALUES(1,?1,?2,0)",
        params![&call[..], &region[..]],
    )
    .unwrap();
    db.execute(
        "INSERT INTO graph_projections(gid,vid,language,graph_hash,state,class_extraction_state,
         class_extraction_payload,class_summary_payload)
         VALUES(2,1,'java',?1,'ready','ready','{}','{}')",
        [&graph_hash],
    )
    .unwrap();
    db.execute(
        "INSERT INTO class_projections(cid,gid,content_hash,state) VALUES(3,2,?1,'ready')",
        [&class_hash],
    )
    .unwrap();
    for (id, name) in [(&owner[..], "Widget"), (&target[..], "Other")] {
        db.execute(
            "INSERT INTO classes(cid,gid,id,name,qualified_name,path,payload)
             VALUES(3,2,?1,?2,'app.Widget','widget.py','{}')",
            params![id, name],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO class_relations(cid,id,owner,target,payload) VALUES(3,'rel-1',?1,?2,'{}')",
        params![&owner[..], &target[..]],
    )
    .unwrap();

    for (ordinal, spelling, lookup_key) in [(0, "name", None), (1, "Ａ", Some("A"))] {
        db.execute(
            "INSERT INTO native_version_references VALUES(1,?1,?2,?3,0,4,?4,?5)",
            params![
                &[ordinal as u8; 16][..],
                &owner[..],
                ordinal,
                spelling,
                lookup_key
            ],
        )
        .unwrap();
    }

    // Compute the former text-key cost from decoded handles and fixture values,
    // not from the budget SQL or SQLite BLOB lengths. Integer fields are charged
    // as decimal text, as they were on main.
    let sid = id_codec::decode_sid(&owner).unwrap();
    let target_sid = id_codec::decode_sid(&target).unwrap();
    let call_id = id_codec::decode_occ(&call).unwrap();
    let region_id = id_codec::decode_occ(&region).unwrap();
    let bytes = |fields: &[&str]| fields.iter().map(|field| field.len()).sum::<usize>();
    let decimal = |fields: &[i64]| {
        fields
            .iter()
            .map(|field| field.to_string().len())
            .sum::<usize>()
    };
    let class_projection_id = class_hash.len() + 20;
    let graph_projection_id = graph_hash.len() + 9;
    let rows = [
        version.len() + bytes(&["supported", "definition"]) + decimal(&[0]),
        version.len()
            + sid.len()
            + bytes(&["function", "héllo", "hello"])
            + decimal(&[1, 0, 0, 0, 0, 9, 0, 5]),
        version.len()
            + call_id.len()
            + sid.len()
            + decimal(&[0, 0, 7])
            + source_set.len()
            + bytes(&["javascript", "hello"]),
        version.len() + region_id.len() + sid.len() + decimal(&[0, 0, 7]) + bytes(&["if", "then"]),
        version.len() + call_id.len() + region_id.len() + decimal(&[0]),
        version.len() + call_id.len() + sid.len() + decimal(&[0, 0, 4]) + bytes(&["name", "name"]),
        version.len() + call_id.len() + sid.len() + decimal(&[1, 0, 4]) + bytes(&["Ａ", "A"]),
        class_projection_id
            + graph_projection_id
            + sid.len()
            + bytes(&["Widget", "app.Widget", "widget.py", "{}"]),
        class_projection_id
            + graph_projection_id
            + target_sid.len()
            + bytes(&["Other", "app.Widget", "widget.py", "{}"]),
        class_projection_id + bytes(&["rel-1", "{}"]) + sid.len() + target_sid.len(),
    ];
    let actual = Store::selected_ancillary_byte_usage(&db, 1, 3, version, source_set).unwrap();
    assert_eq!(
        actual,
        (
            *rows.iter().max().unwrap() as i64,
            rows.iter().sum::<usize>() as i64,
        )
    );
}

#[cfg(test)]
#[test]
fn immutable_reference_slot_rejects_changed_payload() {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch(CACHE_SCHEMA_V29).unwrap();
    db.pragma_update(None, "foreign_keys", "OFF").unwrap();
    let owner = [0x11u8; 16];
    let original = [0x22u8; 16];
    let changed = [0x33u8; 16];
    let sql = "INSERT INTO native_version_references VALUES(?1,?2,?3,?4,?5,?6,?7,?8)";
    let mut append = ImmutableAppend::default();
    for id in [&original[..], &original[..]] {
        append
            .insert(
                &db,
                sql,
                params![1, id, &owner[..], 0, 0, 4, "name", None::<&str>],
            )
            .unwrap();
    }
    let error = append
        .insert(
            &db,
            sql,
            params![1, &changed[..], &owner[..], 0, 0, 4, "name", None::<&str>],
        )
        .unwrap_err();
    assert!(
        error.downcast_ref::<ControlIntegrity>().is_some(),
        "{error:#}"
    );
    assert_eq!(
        db.query_row("SELECT count(*) FROM native_version_references", [], |r| {
            r.get::<_, i64>(0)
        })
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row("SELECT id FROM native_version_references", [], |r| r
            .get::<_, Vec<u8>>(0))
            .unwrap(),
        original
    );
}

#[cfg(test)]
#[test]
fn architecture_retained_package_components_fail_atomically_with_typed_integrity() {
    for fault in ["missing", "duplicate", "length", "hex"] {
        let (_state, _root, store) = selected_read_once_tests::published_store(1);
        let db = store.cache_write().unwrap();
        let (id, retained): (String, String) = db
            .query_row(
                "SELECT id,retained_dependencies FROM native_revisions LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let mut inputs: Vec<serde_json::Value> = serde_json::from_str(&retained).unwrap();
        let index = inputs
            .iter()
            .position(|input| input["path"] == "package.json")
            .unwrap();
        match fault {
            "missing" => {
                inputs.remove(index);
            }
            "duplicate" => {
                inputs.push(inputs[index].clone());
            }
            "length" => {
                inputs[index] = serde_json::json!({"type":"present", "path":"package.json", "bytes":"7b", "byteLength":2});
            }
            "hex" => {
                inputs[index] = serde_json::json!({"type":"present", "path":"package.json", "bytes":"zz", "byteLength":1});
            }
            _ => unreachable!(),
        }
        db.execute(
            "UPDATE native_revisions SET retained_dependencies=?1 WHERE id=?2",
            params![serde_json::to_string(&inputs).unwrap(), id],
        )
        .unwrap();
        drop(db);
        let response = store.evidence_response().unwrap();
        let error = response
            .architecture_response("references", &serde_json::json!({}), &mut || Ok(()))
            .unwrap_err();
        assert!(error.is::<SelectedIntegrity>(), "{fault}: {error:#}");
    }
}

#[cfg(test)]
mod selected_manifest_query_plan_tests;

#[cfg(test)]
mod rebaseline_fault_tests;
#[cfg(test)]
mod recreate_spool_tests;

#[cfg(test)]
mod sqlite_schema_race_tests;

#[cfg(test)]
mod corruption_recovery_tests;
#[cfg(test)]
mod metrics_parallel_tests;
#[cfg(test)]
mod prior_corruption_tests;
#[cfg(test)]
mod review_recovery_tests;
#[cfg(test)]
mod review_typed_recovery_tests;
#[cfg(test)]
mod selected_read_once_tests;
#[cfg(test)]
mod selected_source_budget_tests;

#[cfg(test)]
mod busy_contention_classifier_tests;

#[cfg(test)]
mod live_sqlite_witness_tests;

#[cfg(test)]
mod sqlite_deleted_witness_tests;

#[cfg(test)]
mod catalog_summary_tests;
#[cfg(test)]
mod preflight_hash_reuse_tests;
#[cfg(test)]
mod unchanged_trust_tests;
