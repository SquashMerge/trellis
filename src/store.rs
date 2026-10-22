//! SQLite snapshots and durable user data. Connections are never shared between threads.
pub mod anchors;
pub(crate) mod mcp_read;
pub mod requests;
pub(crate) mod symbol_trigrams;
pub mod topology;
use crate::model::*;
use anyhow::{Context, Result, ensure};
pub use requests::{MaintenanceQueueProbe, MaintenanceQueueState, QueueProbeAdmission};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
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
    /// Its attestation and metrics work is starting.
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
    /// Confirmed terminal request COMMITs on this queue, by any Store instance
    /// (Decision 0009 §8). A waiter records it, re-reads its row, and waits
    /// for it to change; requests.db stays the only record of outcomes.
    terminal_commits: Mutex<u64>,
    terminal_changed: Condvar,
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
    obsolete_format_marker: Arc<Mutex<Option<IndexFormatMarker>>>,
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
    #[cfg(feature = "test-support")]
    test_after_claim_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_before_request_terminal_commit_hook: Arc<TestOneShotHook>,
    maintenance_before_writer_hook: Arc<TestOneShotHook>,
    maintenance_after_first_delete_hook: Arc<TestOneShotHook>,
    publication_before_commit_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_stream_before_preflight_hook: Arc<Mutex<Option<TestStreamPreflightHook>>>,
    /// The publication page cache in KiB; only tests lower it.
    publication_cache_kib: Arc<std::sync::atomic::AtomicI64>,
    writer_before_metadata_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_queue_before_shared_hook: Arc<TestOneShotHook>,
    /// Called with each metrics document's path as its attestation and metrics work starts.
    #[cfg(test)]
    test_metrics_document_hook: Arc<Mutex<Option<TestMetricsDocumentHook>>>,
    #[cfg(feature = "test-support")]
    test_queue_after_root_verify_hook: Arc<TestOneShotHook>,
    #[cfg(test)]
    test_queue_select_hook: Arc<TestOneShotHook>,
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
#[derive(Debug)]
struct ObsoleteIndexFormat(IndexFormatMarker);
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
struct SelectedIntegrity(String);
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
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.role_kind AS BLOB)),0)+COALESCE(length(CAST(x.role AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0) AS row_bytes FROM native_version_coverage_roles x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.owner_syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.kind AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.lookup_key AS BLOB)),0)+COALESCE(length(CAST(x.key_signature_present AS BLOB)),0)+COALESCE(length(CAST(x.key_type_parameter_count AS BLOB)),0)+COALESCE(length(CAST(x.key_variadic AS BLOB)),0)+COALESCE(length(CAST(x.key_ordinal AS BLOB)),0)+COALESCE(length(CAST(x.start_byte AS BLOB)),0)+COALESCE(length(CAST(x.end_byte AS BLOB)),0)+COALESCE(length(CAST(x.name_start AS BLOB)),0)+COALESCE(length(CAST(x.name_end AS BLOB)),0) AS row_bytes FROM native_version_declarations x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.kind AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.sibling_ordinal AS BLOB)),0)+COALESCE(length(CAST(x.signature_present AS BLOB)),0)+COALESCE(length(CAST(x.type_parameter_count AS BLOB)),0)+COALESCE(length(CAST(x.variadic AS BLOB)),0) AS row_bytes FROM native_version_declaration_ancestors x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.type_name AS BLOB)),0) AS row_bytes FROM native_version_own_signature_types x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.ancestor_ordinal AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.type_name AS BLOB)),0) AS row_bytes FROM native_version_ancestor_signature_types x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.kind AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.result_type AS BLOB)),0) AS row_bytes FROM native_version_headers x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.item_kind AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.value AS BLOB)),0) AS row_bytes FROM native_version_header_items x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.type_name AS BLOB)),0)+COALESCE(length(CAST(x.variadic AS BLOB)),0) AS row_bytes FROM native_version_parameters x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.id AS BLOB)),0)+COALESCE(length(CAST(x.owner_syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.start_byte AS BLOB)),0)+COALESCE(length(CAST(x.end_byte AS BLOB)),0)+COALESCE(length(CAST(x.callee_start AS BLOB)),0)+COALESCE(length(CAST(x.callee_end AS BLOB)),0)+COALESCE(length(CAST(x.spelling AS BLOB)),0)+COALESCE(length(CAST(x.source_set_id AS BLOB)),0)+COALESCE(length(CAST(x.language AS BLOB)),0)+COALESCE(length(CAST(x.callee_lookup_key AS BLOB)),0) AS row_bytes FROM native_version_calls x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.id AS BLOB)),0)+COALESCE(length(CAST(x.owner_syntax_id AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0)+COALESCE(length(CAST(x.kind AS BLOB)),0)+COALESCE(length(CAST(x.start_byte AS BLOB)),0)+COALESCE(length(CAST(x.end_byte AS BLOB)),0)+COALESCE(length(CAST(x.parent_id AS BLOB)),0)+COALESCE(length(CAST(x.arm AS BLOB)),0) AS row_bytes FROM native_version_control_regions x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.version_id AS BLOB)),0)+COALESCE(length(CAST(x.call_id AS BLOB)),0)+COALESCE(length(CAST(x.region_id AS BLOB)),0)+COALESCE(length(CAST(x.ordinal AS BLOB)),0) AS row_bytes FROM native_version_call_regions x WHERE x.version_id=?1
UNION ALL
SELECT COALESCE(length(CAST(x.projection_id AS BLOB)),0)+COALESCE(length(CAST(x.graph_projection_id AS BLOB)),0)+COALESCE(length(CAST(x.id AS BLOB)),0)+COALESCE(length(CAST(x.name AS BLOB)),0)+COALESCE(length(CAST(x.qualified_name AS BLOB)),0)+COALESCE(length(CAST(x.path AS BLOB)),0)+COALESCE(length(CAST(x.payload AS BLOB)),0) AS row_bytes FROM classes x WHERE x.projection_id=?2
UNION ALL
SELECT COALESCE(length(CAST(x.projection_id AS BLOB)),0)+COALESCE(length(CAST(x.id AS BLOB)),0)+COALESCE(length(CAST(x.owner AS BLOB)),0)+COALESCE(length(CAST(x.target AS BLOB)),0)+COALESCE(length(CAST(x.payload AS BLOB)),0) AS row_bytes FROM class_relations x WHERE x.projection_id=?2
)"#;
const DATABASE_SCHEMA_VERSION: u32 = 11;
const EXTRACTOR_VERSION: &str = "native-v6-delta-v1";
const EVIDENCE_FORMAT: &str = "terminal-native-graph-v1";
// The original v8 supersession extension stays byte-for-byte compatible.
const SUPERSESSION_SCHEMA_V8: &str = "CREATE TABLE native_revision_supersessions(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id),superseded_at INTEGER NOT NULL CHECK(superseded_at BETWEEN 0 AND 9007199254740991));";
// Additive v8 maintenance extension: installed under a verified leader's
// write transaction without replacing existing pins, queues, or index inode.
const RETENTION_SCHEMA_V8: &str = r#"
ALTER TABLE native_revision_supersessions ADD COLUMN state TEXT NOT NULL DEFAULT 'retained' CHECK(state IN ('retained','pending','released'));
CREATE INDEX native_supersessions_clock ON native_revision_supersessions(superseded_at,revision_id);
CREATE INDEX native_supersessions_due ON native_revision_supersessions(superseded_at,revision_id) WHERE state='retained';
CREATE TABLE native_revision_release_debt(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,phase TEXT NOT NULL CHECK(phase='pending'));
CREATE TABLE native_release_candidate_versions(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,id TEXT NOT NULL,PRIMARY KEY(revision_id,id));
CREATE TABLE native_release_candidate_graphs(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,id TEXT NOT NULL,PRIMARY KEY(revision_id,id));
CREATE TABLE native_release_candidate_classes(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,id TEXT NOT NULL,PRIMARY KEY(revision_id,id));
"#;
// These indexes avoid quadratic self-FK checks when a large document is retired.
// Keep them separate so indexes written by the previous v8 extension remain valid.
const RETENTION_FK_INDEX_SCHEMA_V8: &str = r#"
CREATE INDEX native_version_declarations_owner ON native_version_declarations(version_id,owner_syntax_id);
CREATE INDEX native_version_regions_parent ON native_version_control_regions(version_id,parent_id,owner_syntax_id);
"#;
// Additive v8 index: validation can read headers without loading source_inventory.
const REVISION_HEADER_INDEX_SCHEMA_V8: &str = "CREATE INDEX native_revisions_header_cover ON native_revisions(source_set_id,published_index_revision,id);";
// Additive v8 index: per-file (revision_id,path) lookups are point reads, not revision scans.
const REVISION_DOCUMENT_PATH_INDEX_SCHEMA_V8: &str =
    "CREATE INDEX revision_documents_path ON revision_documents(revision_id,path);";

const CACHE_SCHEMA_V8: &str = r#"
CREATE TABLE index_metadata(singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema_version INTEGER NOT NULL CHECK(schema_version=11), extractor_version TEXT NOT NULL CHECK(extractor_version='native-v6-delta-v1'), root_spelling TEXT NOT NULL, root_device TEXT NOT NULL, root_inode TEXT NOT NULL, index_generation TEXT NOT NULL, index_revision INTEGER NOT NULL CHECK(index_revision BETWEEN 0 AND 9007199254740991), last_opened_at INTEGER NOT NULL CHECK(last_opened_at BETWEEN 0 AND 9007199254740991), indexed_at TEXT NOT NULL, stats TEXT NOT NULL, diagnostics TEXT NOT NULL, reconcile_options TEXT);
CREATE TABLE native_producers(id TEXT NOT NULL,version TEXT NOT NULL,executable_hash TEXT NOT NULL CHECK(length(executable_hash)=64),kind TEXT NOT NULL CHECK(kind='native'),position_encoding TEXT NOT NULL CHECK(position_encoding='utf8'),PRIMARY KEY(id,version));
CREATE TABLE native_producer_languages(producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,language TEXT NOT NULL,inventory_authenticated INTEGER NOT NULL CHECK(inventory_authenticated IN (0,1)),ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(producer_id,producer_version,language),UNIQUE(producer_id,producer_version,ordinal),FOREIGN KEY(producer_id,producer_version) REFERENCES native_producers(id,version) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_producer_inputs(producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,language TEXT NOT NULL,component_name TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(producer_id,producer_version,language,component_name),UNIQUE(producer_id,producer_version,language,ordinal),FOREIGN KEY(producer_id,producer_version,language) REFERENCES native_producer_languages(producer_id,producer_version,language) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_source_sets(id TEXT PRIMARY KEY,root_id TEXT NOT NULL);
CREATE TABLE native_source_set_languages(source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,language TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(source_set_id,language),UNIQUE(source_set_id,ordinal));
CREATE TABLE native_source_set_dependencies(source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,dependency_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(source_set_id,dependency_id),UNIQUE(source_set_id,ordinal));
CREATE TABLE native_revisions(id TEXT PRIMARY KEY,source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,toolchain_hash TEXT NOT NULL CHECK(length(toolchain_hash)=64),config_hash TEXT NOT NULL CHECK(length(config_hash)=64),dependency_hash TEXT NOT NULL CHECK(length(dependency_hash)=64),native_revision_id TEXT NOT NULL,source_inventory TEXT NOT NULL,dependency_observations TEXT NOT NULL,reconcile_options TEXT,class_warnings TEXT NOT NULL,class_truncated INTEGER NOT NULL CHECK(class_truncated IN (0,1)),graph_stats TEXT NOT NULL CHECK(json_valid(graph_stats) AND json_type(graph_stats)='object'),graph_diagnostics TEXT NOT NULL CHECK(json_valid(graph_diagnostics) AND json_type(graph_diagnostics)='array'),published_index_revision INTEGER NOT NULL CHECK(published_index_revision BETWEEN 0 AND 9007199254740991));
CREATE UNIQUE INDEX native_revisions_pin ON native_revisions(published_index_revision);
CREATE TABLE revision_capture_inputs(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,input_key TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(revision_id,input_key));
CREATE TABLE document_versions(id TEXT PRIMARY KEY,source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,language TEXT NOT NULL,path TEXT NOT NULL,content_hash TEXT NOT NULL CHECK(length(content_hash)=64),extraction_context TEXT NOT NULL CHECK(length(extraction_context)=64),producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,byte_length INTEGER NOT NULL CHECK(byte_length>=0 AND byte_length=length(source_bytes)),source_bytes BLOB NOT NULL,native_witness TEXT NOT NULL CHECK(length(native_witness)=64),UNIQUE(source_set_id,language,path,content_hash,extraction_context,producer_id,producer_version),UNIQUE(id,source_set_id,language,path),UNIQUE(id,language),FOREIGN KEY(producer_id,producer_version,language) REFERENCES native_producer_languages(producer_id,producer_version,language) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX document_versions_path ON document_versions(source_set_id,language,path,id);
CREATE TABLE graph_projections(id TEXT PRIMARY KEY,document_version_id TEXT NOT NULL,language TEXT NOT NULL,graph_hash TEXT NOT NULL CHECK(length(graph_hash)=64),state TEXT NOT NULL CHECK(state IN ('staged','ready','unavailable')),class_extraction_state TEXT NOT NULL CHECK(class_extraction_state IN ('notApplicable','ready')),class_extraction_payload TEXT,CHECK((class_extraction_state='ready')=(class_extraction_payload IS NOT NULL)),CHECK((language IN ('java','python'))=(class_extraction_state IN ('ready'))),UNIQUE(document_version_id,graph_hash),UNIQUE(id,document_version_id),FOREIGN KEY(document_version_id,language) REFERENCES document_versions(id,language) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX graph_projections_version ON graph_projections(document_version_id,id);
CREATE TABLE graph_nodes(projection_id TEXT NOT NULL REFERENCES graph_projections(id) DEFERRABLE INITIALLY DEFERRED,id TEXT NOT NULL,name TEXT NOT NULL,path TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(projection_id,id));
CREATE INDEX graph_nodes_name ON graph_nodes(projection_id,name,id);
CREATE VIRTUAL TABLE graph_node_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');
CREATE TRIGGER graph_node_trigrams_insert AFTER INSERT ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TRIGGER graph_node_trigrams_delete AFTER DELETE ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(graph_node_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); END;
CREATE TRIGGER graph_node_trigrams_update AFTER UPDATE OF name ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(graph_node_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); INSERT INTO graph_node_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TABLE graph_calls(projection_id TEXT NOT NULL,id TEXT NOT NULL,caller TEXT NOT NULL,target TEXT,path TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(projection_id,id),FOREIGN KEY(projection_id,caller) REFERENCES graph_nodes(projection_id,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX graph_calls_caller ON graph_calls(projection_id,caller,id);
CREATE TABLE graph_regions(projection_id TEXT NOT NULL,id TEXT NOT NULL,owner TEXT NOT NULL,path TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(projection_id,id),FOREIGN KEY(projection_id,owner) REFERENCES graph_nodes(projection_id,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX graph_regions_owner ON graph_regions(projection_id,owner,id);
CREATE TABLE class_projections(id TEXT PRIMARY KEY,graph_projection_id TEXT NOT NULL REFERENCES graph_projections(id) DEFERRABLE INITIALLY DEFERRED,content_hash TEXT NOT NULL CHECK(length(content_hash)=64),state TEXT NOT NULL CHECK(state IN ('ready','unavailable')),UNIQUE(graph_projection_id,content_hash),UNIQUE(id,graph_projection_id));
CREATE TABLE classes(projection_id TEXT NOT NULL,graph_projection_id TEXT NOT NULL,id TEXT NOT NULL,name TEXT NOT NULL,qualified_name TEXT NOT NULL,path TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(projection_id,id),FOREIGN KEY(projection_id,graph_projection_id) REFERENCES class_projections(id,graph_projection_id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(graph_projection_id,id) REFERENCES graph_nodes(projection_id,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX classes_path ON classes(projection_id,path,id);
CREATE TABLE class_relations(projection_id TEXT NOT NULL,id TEXT NOT NULL,owner TEXT NOT NULL,target TEXT,payload TEXT NOT NULL,PRIMARY KEY(projection_id,id),FOREIGN KEY(projection_id,owner) REFERENCES classes(projection_id,id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(projection_id,target) REFERENCES classes(projection_id,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX class_relations_owner ON class_relations(projection_id,owner,id);
CREATE INDEX class_relations_target ON class_relations(projection_id,target,id);
CREATE TABLE revision_documents(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,source_set_id TEXT NOT NULL,language TEXT NOT NULL,path TEXT NOT NULL,document_version_id TEXT NOT NULL,graph_projection_id TEXT NOT NULL,class_projection_id TEXT,capture_stat TEXT,coverage_requested INTEGER NOT NULL CHECK(coverage_requested IN (0,1)),coverage_selected INTEGER NOT NULL CHECK(coverage_selected IN (0,1)),coverage_state TEXT NOT NULL CHECK(coverage_state IN ('notRequested','omitted','unsupported','failed','partial','complete')),coverage_diagnostic TEXT,ordinal INTEGER NOT NULL CHECK(ordinal>=0),CHECK((coverage_state='complete')=(coverage_diagnostic IS NULL)),PRIMARY KEY(revision_id,source_set_id,language,path),UNIQUE(revision_id,ordinal),FOREIGN KEY(document_version_id,source_set_id,language,path) REFERENCES document_versions(id,source_set_id,language,path) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(graph_projection_id,document_version_id) REFERENCES graph_projections(id,document_version_id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(class_projection_id,graph_projection_id) REFERENCES class_projections(id,graph_projection_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX revision_documents_version ON revision_documents(document_version_id,revision_id);
CREATE INDEX revision_documents_graph ON revision_documents(graph_projection_id,revision_id);
CREATE INDEX revision_documents_class ON revision_documents(class_projection_id,revision_id);
CREATE TABLE native_version_coverage_roles(version_id TEXT NOT NULL REFERENCES document_versions(id) DEFERRABLE INITIALLY DEFERRED,role_kind TEXT NOT NULL CHECK(role_kind IN ('supported','observed')),role TEXT NOT NULL CHECK(role IN ('definition','call')),ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(version_id,role_kind,ordinal),UNIQUE(version_id,role_kind,role));
CREATE TABLE native_version_declarations(version_id TEXT NOT NULL REFERENCES document_versions(id) DEFERRABLE INITIALLY DEFERRED,syntax_id TEXT NOT NULL,owner_syntax_id TEXT,kind TEXT NOT NULL,role TEXT NOT NULL CHECK(role IN ('test','production')),name TEXT,lookup_key TEXT,key_signature_present INTEGER NOT NULL CHECK(key_signature_present IN (0,1)),key_type_parameter_count INTEGER,key_variadic INTEGER,key_ordinal INTEGER NOT NULL CHECK(key_ordinal>=0),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,name_start INTEGER,name_end INTEGER,CHECK(start_byte>=0 AND end_byte>=start_byte),CHECK((name IS NULL)=(lookup_key IS NULL) AND (name IS NULL)=(name_start IS NULL) AND (name_start IS NULL)=(name_end IS NULL)),CHECK(name_start IS NULL OR (name_start>=start_byte AND name_end<=end_byte AND name_end>name_start)),CHECK((key_signature_present=0 AND key_type_parameter_count IS NULL AND key_variadic IS NULL) OR (key_signature_present=1 AND key_type_parameter_count>=0 AND key_variadic IN (0,1))),PRIMARY KEY(version_id,syntax_id),FOREIGN KEY(version_id,owner_syntax_id) REFERENCES native_version_declarations(version_id,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_declarations_lookup ON native_version_declarations(lookup_key,version_id,syntax_id);
CREATE VIRTUAL TABLE native_declaration_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');
CREATE TRIGGER native_declaration_trigrams_insert AFTER INSERT ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TRIGGER native_declaration_trigrams_delete AFTER DELETE ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(native_declaration_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); END;
CREATE TRIGGER native_declaration_trigrams_update AFTER UPDATE OF name ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(native_declaration_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); INSERT INTO native_declaration_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TABLE native_version_declaration_ancestors(version_id TEXT NOT NULL,syntax_id TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),kind TEXT NOT NULL,name TEXT,sibling_ordinal INTEGER NOT NULL CHECK(sibling_ordinal>=0),signature_present INTEGER NOT NULL CHECK(signature_present IN (0,1)),type_parameter_count INTEGER,variadic INTEGER,CHECK((signature_present=0 AND type_parameter_count IS NULL AND variadic IS NULL) OR (signature_present=1 AND type_parameter_count>=0 AND variadic IN (0,1))),PRIMARY KEY(version_id,syntax_id,ordinal),FOREIGN KEY(version_id,syntax_id) REFERENCES native_version_declarations(version_id,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_own_signature_types(version_id TEXT NOT NULL,syntax_id TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),type_name TEXT NOT NULL,PRIMARY KEY(version_id,syntax_id,ordinal),FOREIGN KEY(version_id,syntax_id) REFERENCES native_version_declarations(version_id,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_ancestor_signature_types(version_id TEXT NOT NULL,syntax_id TEXT NOT NULL,ancestor_ordinal INTEGER NOT NULL CHECK(ancestor_ordinal>=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),type_name TEXT NOT NULL,PRIMARY KEY(version_id,syntax_id,ancestor_ordinal,ordinal),FOREIGN KEY(version_id,syntax_id,ancestor_ordinal) REFERENCES native_version_declaration_ancestors(version_id,syntax_id,ordinal) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_headers(version_id TEXT NOT NULL,syntax_id TEXT NOT NULL,kind TEXT NOT NULL,name TEXT,result_type TEXT,PRIMARY KEY(version_id,syntax_id),FOREIGN KEY(version_id,syntax_id) REFERENCES native_version_declarations(version_id,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_header_items(version_id TEXT NOT NULL,syntax_id TEXT NOT NULL,item_kind TEXT NOT NULL CHECK(item_kind IN ('modifier','typeParameter','base')),ordinal INTEGER NOT NULL CHECK(ordinal>=0),value TEXT NOT NULL,PRIMARY KEY(version_id,syntax_id,item_kind,ordinal),FOREIGN KEY(version_id,syntax_id) REFERENCES native_version_headers(version_id,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_parameters(version_id TEXT NOT NULL,syntax_id TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),name TEXT,type_name TEXT,variadic INTEGER NOT NULL CHECK(variadic IN (0,1)),PRIMARY KEY(version_id,syntax_id,ordinal),FOREIGN KEY(version_id,syntax_id) REFERENCES native_version_headers(version_id,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_calls(version_id TEXT NOT NULL,id TEXT NOT NULL,owner_syntax_id TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,callee_start INTEGER,callee_end INTEGER,spelling TEXT,source_set_id TEXT NOT NULL,language TEXT NOT NULL,callee_lookup_key TEXT,CHECK(start_byte>=0 AND end_byte>start_byte),CHECK((callee_start IS NULL)=(callee_end IS NULL)),CHECK(callee_start IS NULL OR (callee_start>=start_byte AND callee_end<=end_byte AND callee_end>callee_start)),PRIMARY KEY(version_id,id),UNIQUE(version_id,owner_syntax_id,ordinal),FOREIGN KEY(version_id,owner_syntax_id) REFERENCES native_version_declarations(version_id,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_calls_owner ON native_version_calls(version_id,owner_syntax_id,ordinal);
CREATE INDEX native_version_calls_incoming ON native_version_calls(source_set_id,language,callee_lookup_key);
CREATE TABLE native_version_control_regions(version_id TEXT NOT NULL,id TEXT NOT NULL,owner_syntax_id TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),kind TEXT NOT NULL,start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,parent_id TEXT,arm TEXT,CHECK(start_byte>=0 AND end_byte>start_byte),PRIMARY KEY(version_id,id),UNIQUE(version_id,owner_syntax_id,ordinal),UNIQUE(version_id,id,owner_syntax_id),FOREIGN KEY(version_id,owner_syntax_id) REFERENCES native_version_declarations(version_id,syntax_id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(version_id,parent_id,owner_syntax_id) REFERENCES native_version_control_regions(version_id,id,owner_syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_regions_owner ON native_version_control_regions(version_id,owner_syntax_id,ordinal);
CREATE TABLE native_version_call_regions(version_id TEXT NOT NULL,call_id TEXT NOT NULL,region_id TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(version_id,call_id,ordinal),UNIQUE(version_id,call_id,region_id),FOREIGN KEY(version_id,call_id) REFERENCES native_version_calls(version_id,id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(version_id,region_id) REFERENCES native_version_control_regions(version_id,id) DEFERRABLE INITIALLY DEFERRED);
"#;
// Additive schema8 extension. An existing v8 cache keeps every row, generation,
// pin, and queue; the first new publication installs these two tables atomically.
const PRODUCER_BINDING_SCHEMA_V8: &str = r#"
CREATE TABLE native_binding_epoch(singleton INTEGER PRIMARY KEY CHECK(singleton=1),index_generation TEXT NOT NULL,first_revision INTEGER NOT NULL CHECK(first_revision BETWEEN 1 AND 9007199254740991));
CREATE TABLE revision_producer_bindings(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,producer_sha TEXT NOT NULL CHECK(length(producer_sha)=64),binding_sha TEXT NOT NULL CHECK(length(binding_sha)=64));
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
    attestations: usize,
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

/// What `selected_source_witness_with_progress` reparses one selected source under.
struct WitnessInputs {
    producer: crate::native_evidence::Producer,
    source_set: crate::native_evidence::SourceSet,
    revision: crate::native_evidence::Revision,
}

/// One selected document's manifest row, after its SQL row and byte budgets
/// passed and before the rows they bound are read.
struct SelectedHeader {
    path: String,
    language: String,
    version_id: String,
    graph_id: String,
    class_id: String,
    graph_hash: String,
    class_hash: Option<String>,
    hash: String,
    bytes: i64,
    scope: V8NativeScope,
    /// The source, native and class ancillary rows, graph rows and F this
    /// document's rows hold, in stored bytes, as its budgets measured them.
    stored_bytes: u64,
}

/// One selected document's stored rows, read from one snapshot by
/// `Store::fetch_selected_document` and not yet compared with its source.
struct FetchedDocument {
    language: String,
    version_id: String,
    graph_id: String,
    class_id: String,
    graph_hash: String,
    class_hash: Option<String>,
    file: SourceFile,
    witness: WitnessInputs,
    coverage: crate::native_evidence::Coverage,
    declarations: Vec<crate::native_evidence::Declaration>,
    calls: Vec<crate::native_evidence::Call>,
    regions: Vec<crate::native_evidence::ControlRegion>,
    node_rows: Vec<(String, String, String, String)>,
    call_rows: Vec<(String, String, Option<String>, String, String)>,
    region_rows: Vec<(String, String, String, String)>,
    class_rows: Vec<(String, String, String, String, String)>,
    relation_rows: Vec<(String, String, Option<String>, String)>,
    stored_f: Option<String>,
}

/// One selected document's source and native rows, as authenticated by its
/// attestation in the caller's snapshot.
struct AttestedDocument {
    file: SourceFile,
    declarations: Vec<crate::native_evidence::Declaration>,
    calls: Vec<crate::native_evidence::Call>,
    regions: Vec<crate::native_evidence::ControlRegion>,
}

/// Immutable revision chosen inside an admitted read transaction. This is not
/// a surrogate for control metadata: status, root and schema remain live-only.
struct ReadRevision {
    pin: IndexPin,
    key: String,
    /// Header rows of this immutable selected revision, read once per snapshot
    /// and shared by every document attested in it.
    headers: Arc<SelectedHeaders>,
}
#[derive(Default)]
struct SelectedHeaders {
    producer_hash: OnceLock<String>,
    witness: OnceLock<WitnessHeaders>,
}
/// The producer, source set and revision a selected witness is measured under.
struct WitnessHeaders {
    producer: crate::native_evidence::Producer,
    source_set: crate::native_evidence::SourceSet,
    revision: crate::native_evidence::Revision,
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
        )?;
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
    pub scope: String,
    pub source_set_id: Option<String>,
    pub language: Option<String>,
    pub path: Option<String>,
    pub sid: Option<String>,
    pub aggregate: String,
    /// Empty means all metrics; transport rejects an explicitly empty list.
    pub metrics: Vec<String>,
}
impl Default for MetricsOptions {
    fn default() -> Self {
        Self {
            pin: None,
            scope: "revision".into(),
            source_set_id: None,
            language: None,
            path: None,
            sid: None,
            aggregate: "sum".into(),
            metrics: vec![],
        }
    }
}
impl MetricsOptions {
    pub fn validate(&self) -> Result<()> {
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
    pub declaration_count: usize,
    pub metrics: serde_json::Value,
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
    pub source_set_id: String,
    pub language: String,
    pub path: String,
    pub sid: String,
    pub range: crate::native_evidence::Range,
    pub metrics: serde_json::Value,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricsPayload {
    pub metrics_version: &'static str,
    pub index_generation: String,
    pub index_revision: u64,
    pub scope: String,
    pub aggregate: String,
    pub selected_metrics: Vec<String>,
    pub declarations: Vec<MetricsDeclarationRow>,
    pub files: Vec<MetricsFileRow>,
    pub directories: Vec<MetricsDirectoryRow>,
}

/// One coherent native read snapshot retained until its owned result passes T03.
pub struct EvidenceResponse {
    store: Store,
    db: IndexConnection,
    fence: ReadFence,
}
/// A response owns one validated SQLite snapshot until its root and selected
/// revision have been fenced at finish.
struct ReadFence(IndexPin);
impl ReadFence {
    fn verify(&self, store: &Store, db: Option<&Connection>) -> Result<()> {
        store.identity.verify()?;
        if let Some(db) = db {
            let status = store.read_status_for(db, true)?;
            ensure!(
                status.revision == self.0,
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

/// Why one revision document did not produce metrics.
enum MetricsDocumentFailure {
    /// Attestation failed; reported as a selected-read failure.
    Attest(anyhow::Error),
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
/// Read-ahead budget across the chunk being read and the chunk the workers
/// hold, charged as each document's stored bytes (source, native and class
/// ancillary rows, graph rows and F) as its existing SQL budgets measured them.
/// Each of the two chunks is charged at most half of it; a document charged
/// more than half is read and computed alone.
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
    /// The most read-ahead charge held at once.
    max_resident_bytes: u64,
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
    /// Compute metrics from one selected revision in this response's read transaction.
    pub fn metrics_at(
        &self,
        options: &MetricsOptions,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<MetricsPayload> {
        options.validate()?;
        let progress = MetricsReadCheck {
            check: Mutex::new(check),
            failure: Mutex::new(None),
            stopped: AtomicBool::new(false),
        };
        progress.check()?;
        let selected = self.store.read_revision_for(&self.db, options.pin, true)?;
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
            "SELECT source_set_id,language,path FROM revision_documents
            WHERE revision_id=?1 ORDER BY source_set_id,language,path",
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
        let mut declarations = Vec::new();
        let mut files = Vec::new();
        let mut directories: BTreeMap<(String, String), crate::metrics::Accumulator> =
            BTreeMap::new();
        // Read and attest only the selected version; no checkout filesystem access.
        // Every SQL read runs in order on this request's connection and snapshot.
        // The source comparison and metrics run on workers, a chunk behind the
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
             -> std::result::Result<crate::metrics::Computed, MetricsDocumentFailure> {
                let calls = std::cell::Cell::new(0);
                let expired = || progress.expired_every(&calls, check_every);
                // Metrics use the source and native rows exactly as attestation read them.
                let attested = Store::attest_fetched(fetched, Some(&expired))
                    .map_err(MetricsDocumentFailure::Attest)?;
                if attested.file.language != key.language {
                    return Err(MetricsDocumentFailure::Other(anyhow::anyhow!(
                        "incompatible_index: metrics language mismatch"
                    )));
                }
                if expired() {
                    return Err(MetricsDocumentFailure::Stopped);
                }
                let source = crate::metrics::SourceInput {
                    language: &key.language,
                    content_hash: &attested.file.hash,
                    text: &attested.file.text,
                };
                let cached = store
                    .metrics_memo
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .lookup(&source);
                crate::metrics::compute_with_cached(
                    cached,
                    source,
                    &attested.declarations,
                    &attested.calls,
                    &attested.regions,
                    &mut || {
                        if expired() {
                            anyhow::bail!("cancelled")
                        }
                        Ok(())
                    },
                )
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
            std::result::Result<crate::metrics::Computed, MetricsDocumentFailure>,
        >|
         -> Result<()> {
            let computed = match outcome {
                Some(Ok(computed)) => computed,
                Some(Err(MetricsDocumentFailure::Attest(error))) => {
                    return progress.result(Err(self
                        .store
                        .report_selected_failure(selected_integrity(error))));
                }
                Some(Err(MetricsDocumentFailure::Other(error))) => {
                    return progress.result(Err(error));
                }
                // A document a worker never started was skipped after a check failed.
                Some(Err(MetricsDocumentFailure::Stopped)) | None => {
                    return progress.result(Err(anyhow::anyhow!("cancelled")));
                }
            };
            let rows = computed.commit(
                &mut self
                    .store
                    .metrics_memo
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()),
            )?;
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
                if options.scope == "file"
                    || (options.scope == "declaration" && options.sid.as_ref() == Some(&row.sid))
                {
                    let mut projected = serde_json::to_value(&row.metrics)?;
                    projected
                        .as_object_mut()
                        .expect("metric score object")
                        .retain(|name, _| selected_metrics.iter().any(|selected| selected == name));
                    declarations.push(MetricsDeclarationRow {
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
            let mut carried: Option<(&DocumentKey, SelectedHeader)> = None;
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
                // Each chunk is charged at most half the budget, so the two held
                // at once stay within it. Each document's budgets are read first
                // and its rows only if its charge fits in this chunk. A document
                // over half the budget is read and computed alone.
                let mut keys: Vec<&DocumentKey> = Vec::new();
                let mut fetched: Vec<std::sync::Mutex<Option<Fetched>>> = Vec::new();
                let mut bytes = 0u64;
                let mut alone = false;
                while keys.len() < METRICS_FETCH_DOCUMENTS {
                    let (key, header) = match carried.take() {
                        Some(carry) => carry,
                        None => {
                            if exhausted {
                                break;
                            }
                            let Some(&key) = remaining.next() else {
                                exhausted = true;
                                break;
                            };
                            let header = if progress.expired() {
                                Err(MetricsDocumentFailure::Stopped)
                            } else {
                                self.store
                                    .fetch_selected_header(
                                        &self.db,
                                        &key.path,
                                        &selected,
                                        Some(&expired),
                                    )
                                    .map_err(MetricsDocumentFailure::Attest)
                            };
                            match header {
                                Ok(header) => (key, header),
                                Err(failure) => {
                                    // Nothing after a failed document is read.
                                    keys.push(key);
                                    fetched.push(std::sync::Mutex::new(Some(Err(failure))));
                                    exhausted = true;
                                    break;
                                }
                            }
                        }
                    };
                    let charge = header.stored_bytes;
                    let oversized = charge > chunk_bytes;
                    if !keys.is_empty() && (oversized || bytes.saturating_add(charge) > chunk_bytes)
                    {
                        // Start the next chunk with it.
                        carried = Some((key, header));
                        break;
                    }
                    if oversized && pending.is_some() {
                        // Accept the chunk the workers hold before reading it.
                        carried = Some((key, header));
                        break;
                    }
                    keys.push(key);
                    let item = self
                        .store
                        .fetch_selected_rows(&self.db, header, &selected, Some(&expired))
                        .map_err(MetricsDocumentFailure::Attest);
                    let failed = item.is_err();
                    fetched.push(std::sync::Mutex::new(Some(item)));
                    bytes = bytes.saturating_add(charge);
                    #[cfg(test)]
                    {
                        METRICS_FETCH_TRACE.with_borrow_mut(|trace| trace.charges.push(charge));
                        trace_metrics_resident(
                            fetched.len() + pending.as_ref().map_or(0, |(keys, _)| keys.len()),
                            bytes.saturating_add(pending_bytes),
                        );
                    }
                    if failed {
                        exhausted = true;
                        break;
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
            ensure!(
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
            index_generation: selected.pin.index_generation.to_string(),
            index_revision: selected.pin.index_revision,
            scope: options.scope.clone(),
            aggregate: options.aggregate.clone(),
            selected_metrics,
            declarations,
            files,
            directories: directory_rows,
        })
    }
    pub fn finish<T>(&self, value: T) -> Result<T> {
        self.fence.verify(&self.store, Some(&self.db))?;
        Ok(value)
    }
    pub fn status(&self) -> Result<IndexStatus> {
        self.store.read_status_for(&self.db, true)
    }
    pub fn validate_pin(&self, pin: IndexPin) -> Result<()> {
        self.store.read_revision_for(&self.db, Some(pin), true)?;
        Ok(())
    }
    pub fn source_at(
        &self,
        path: &str,
        expected: Option<IndexPin>,
    ) -> Result<Option<(IndexPin, SourceFile)>> {
        let selected = self.store.read_revision_for(&self.db, expected, true)?;
        let source = self
            .store
            .selected_source_row_for(&self.db, path, &selected)?;
        if source.is_some() {
            self.store
                .attest_selected_document_for(&self.db, path, &selected)?;
        }
        Ok(source.map(|source| (selected.pin, source)))
    }
    pub fn validate_selected_view(&self, view: &ViewResult, sources: &[SourceFile]) -> Result<()> {
        self.store
            .validate_selected_view_in(&self.db, view, sources)
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
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        self.store.read_graph_for(&self.db, &selected)
    }
    pub fn symbol_at(
        &self,
        id: &str,
        expected_revision: Option<IndexPin>,
    ) -> Result<Option<(IndexPin, Symbol)>> {
        self.store.symbol_in(&self.db, id, expected_revision, true)
    }
    fn saved_pin(&self, expected: Option<IndexPin>) -> Result<IndexPin> {
        Ok(self.store.read_revision_for(&self.db, expected, true)?.pin)
    }
    pub fn saved_views_at(&self, expected: Option<IndexPin>) -> Result<Vec<SavedViewState>> {
        let pin = self.saved_pin(expected)?;
        self.store
            .records()
            .view_records()?
            .into_iter()
            .map(|v| Store::resolve_view(&self.db, &self.store, v, Some(pin), true))
            .collect()
    }
    pub fn saved_view_at(
        &self,
        id: &str,
        expected: Option<IndexPin>,
    ) -> Result<Option<SavedViewState>> {
        let pin = self.saved_pin(expected)?;
        self.store
            .records()
            .view_record(id)?
            .map(|v| Store::resolve_view(&self.db, &self.store, v, Some(pin), true))
            .transpose()
    }
    pub fn saved_annotations_at(&self, expected: Option<IndexPin>) -> Result<Vec<AnnotationState>> {
        let pin = self.saved_pin(expected)?;
        self.store
            .records()
            .annotation_records()?
            .into_iter()
            .map(|a| Store::resolve_annotation(&self.db, &self.store, a, Some(pin), true))
            .collect()
    }
    /// A predecessor permit is read-only. Mutations require a current strict
    /// read, with the root and publication fence checked before sidecar work.
    pub fn require_mutation_ready(&self) -> Result<()> {
        self.store.ensure_public_read_ready()?;
        self.finish(())
    }
    pub fn save_view_at(&self, pin: IndexPin, view: &SavedView) -> Result<SavedViewState> {
        view.validate()?;
        self.require_mutation_ready()?;
        self.saved_pin(Some(pin))?;
        ensure!(
            self.status()?.revision == pin,
            "revision conflict: mutation requires head"
        );
        let record = self.store.records().update_view_record(
            &SavedViewRecord::from_base(view.clone(), None),
            || {
                serde_json::value::to_raw_value(&Store::selected_anchor_in(
                    &self.db,
                    &self.store,
                    &view.query.seed,
                    true,
                )?)
                .map_err(Into::into)
            },
        )?;
        Store::resolve_view(&self.db, &self.store, record, Some(pin), true)
    }
    pub fn save_annotation_at(
        &self,
        pin: IndexPin,
        request: &AnnotationRequest,
    ) -> Result<AnnotationState> {
        request.validate()?;
        self.require_mutation_ready()?;
        self.saved_pin(Some(pin))?;
        ensure!(
            self.status()?.revision == pin,
            "revision conflict: mutation requires head"
        );
        let title = request
            .title
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let record = self.store.records().update_annotation_record(
            &AnnotationRecord::from_base(request.base(), title, None),
            request.title.is_none(),
            || {
                serde_json::value::to_raw_value(&Store::selected_anchor_in(
                    &self.db,
                    &self.store,
                    &request.node_id,
                    true,
                )?)
                .map_err(Into::into)
            },
        )?;
        Store::resolve_annotation(&self.db, &self.store, record, Some(pin), true)
    }
    pub fn delete_view(&self, id: &str) -> Result<bool> {
        self.require_mutation_ready()?;
        self.store.delete_view(id)
    }
    pub fn delete_annotation(&self, id: &str) -> Result<bool> {
        self.require_mutation_ready()?;
        self.store.delete_annotation(id)
    }
    /// Ordinary reads release the snapshot before slow response assembly.
    /// A transitional exact-pin fence retains its predecessor snapshot; T03
    /// releases it after materialization so startup reconciliation may commit before finish.
    pub fn into_fence(self, policy: EvidenceFencePolicy) -> EvidenceFence {
        let Self { store, db, fence } = self;
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
        }
    }
}
/// Unpinned responses need T03 only; cached packets also require their exact pin.
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
}
impl EvidenceFence {
    pub fn finish<T>(&self, value: T) -> Result<T> {
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
fn protected_sqlite_open(
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

fn retained_sqlite_file(
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
    type Object = (String, String, String, Option<String>);
    fn objects(db: &Connection) -> Result<Vec<Object>> {
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
    let expected = Connection::open_in_memory()?;
    let version: u32 = storage_result(db.pragma_query_value(None, "user_version", |r| r.get(0)))?;
    ensure!(
        version <= DATABASE_SCHEMA_VERSION,
        "incompatible_index: unsupported newer schema cannot be rebaselined"
    );
    control_ensure!(
        version == DATABASE_SCHEMA_VERSION,
        "incompatible_index: unknown schema version"
    );
    expected.execute_batch(CACHE_SCHEMA_V8)?;
    let actual = objects(db)?;
    if actual == objects(&expected)? {
        return Ok(());
    }
    expected.execute_batch(PRODUCER_BINDING_SCHEMA_V8)?;
    if actual == objects(&expected)? {
        return Ok(());
    }
    expected.execute_batch(SUPERSESSION_SCHEMA_V8)?;
    if actual == objects(&expected)? {
        return Ok(());
    }
    expected.execute_batch(RETENTION_SCHEMA_V8)?;
    if actual == objects(&expected)? {
        return Ok(());
    }
    expected.execute_batch(RETENTION_FK_INDEX_SCHEMA_V8)?;
    if actual == objects(&expected)? {
        return Ok(());
    }
    expected.execute_batch(REVISION_HEADER_INDEX_SCHEMA_V8)?;
    if actual == objects(&expected)? {
        return Ok(());
    }
    expected.execute_batch(REVISION_DOCUMENT_PATH_INDEX_SCHEMA_V8)?;
    if actual == objects(&expected)? {
        return Ok(());
    }
    let without_binding = Connection::open_in_memory()?;
    without_binding.execute_batch(CACHE_SCHEMA_V8)?;
    without_binding.execute_batch(SUPERSESSION_SCHEMA_V8)?;
    if actual == objects(&without_binding)? {
        return Ok(());
    }
    without_binding.execute_batch(RETENTION_SCHEMA_V8)?;
    if actual == objects(&without_binding)? {
        return Ok(());
    }
    without_binding.execute_batch(RETENTION_FK_INDEX_SCHEMA_V8)?;
    if actual == objects(&without_binding)? {
        return Ok(());
    }
    without_binding.execute_batch(REVISION_HEADER_INDEX_SCHEMA_V8)?;
    if actual == objects(&without_binding)? {
        return Ok(());
    }
    without_binding.execute_batch(REVISION_DOCUMENT_PATH_INDEX_SCHEMA_V8)?;
    control_ensure!(
        actual == objects(&without_binding)?,
        "incompatible_index: unknown cache object type, name or shape"
    );
    Ok(())
}
fn has_revision_supersessions(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_revision_supersessions')",
        [], |r| r.get(0),
    )?)
}

fn has_revision_release_debt(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_revision_release_debt')",
        [], |r| r.get(0),
    )?)
}

fn has_retention_fk_indexes(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='native_version_declarations_owner')",
        [], |r| r.get(0),
    )?)
}

fn has_revision_header_cover(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='native_revisions_header_cover')",
        [], |r| r.get(0),
    )?)
}

fn has_revision_document_path_index(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='index' AND name='revision_documents_path')",
        [], |r| r.get(0),
    )?)
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
    let (candidate, parent, reference, children, child_key) = match kind {
        "class" => (
            "native_release_candidate_classes",
            "class_projections",
            "class_projection_id",
            &["class_relations", "classes"][..],
            "projection_id",
        ),
        "graph" => (
            "native_release_candidate_graphs",
            "graph_projections",
            "graph_projection_id",
            &["graph_calls", "graph_regions", "graph_nodes"][..],
            "projection_id",
        ),
        "version" => (
            "native_release_candidate_versions",
            "document_versions",
            "document_version_id",
            &[
                "native_version_call_regions",
                "native_version_calls",
                "native_version_control_regions",
                "native_version_header_items",
                "native_version_parameters",
                "native_version_headers",
                "native_version_ancestor_signature_types",
                "native_version_own_signature_types",
                "native_version_declaration_ancestors",
                "native_version_declarations",
                "native_version_coverage_roles",
            ][..],
            "version_id",
        ),
        _ => unreachable!("fixed cleanup kind"),
    };
    // A release unit can retire many small candidates. Referenced candidates
    // need just the indexed EXISTS probe and an un-journal; large child trees
    // retain the candidate while small committed DELETE batches resume later.
    for _ in 0..64 {
        let candidate_id: Option<String> = tx
            .query_row(
                &format!("SELECT id FROM {candidate} WHERE revision_id=?1 ORDER BY id LIMIT 1"),
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
            [&id],
            |r| r.get(0),
        )?;
        if !still_referenced {
            // A projection absent from the manifest may still own another
            // projection. Journal its dependent before removing the parent.
            let dependent = match kind {
                "graph" => Some((
                    "class_projections",
                    "graph_projection_id",
                    "native_release_candidate_classes",
                )),
                "version" => Some((
                    "graph_projections",
                    "document_version_id",
                    "native_release_candidate_graphs",
                )),
                _ => None,
            };
            if let Some((table, field, queue)) = dependent {
                let child: Option<String> = tx
                    .query_row(
                        &format!("SELECT id FROM {table} WHERE {field}=?1 ORDER BY id LIMIT 1"),
                        [&id],
                        |r| r.get(0),
                    )
                    .optional()?;
                if let Some(child) = child {
                    tx.execute(
                        &format!("INSERT OR IGNORE INTO {queue}(revision_id,id) VALUES(?1,?2)"),
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
                    tx.execute(&format!("DELETE FROM {child} WHERE {child_key}=?1"), [&id])?
                } else {
                    tx.execute(
                        &format!("DELETE FROM {child} WHERE rowid IN (SELECT rowid FROM {child} WHERE {child_key}=?1 LIMIT 256)"),
                        [&id],
                    )?
                };
                if !self_referential && changed == 256 {
                    return Ok(());
                }
            }
            tx.execute(&format!("DELETE FROM {parent} WHERE id=?1"), [&id])?;
        }
        tx.execute(
            &format!("DELETE FROM {candidate} WHERE revision_id=?1 AND id=?2"),
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

// Called only with the leader's immediate write transaction. Existing pins stay put.
fn install_supersessions(db: &Connection, now: i64) -> Result<()> {
    let missing_supersessions = !has_revision_supersessions(db)?;
    let missing_retention = !has_revision_release_debt(db)?;
    if missing_supersessions {
        db.execute_batch(SUPERSESSION_SCHEMA_V8)?;
    }
    if missing_retention {
        db.execute_batch(RETENTION_SCHEMA_V8)?;
    }
    if !has_retention_fk_indexes(db)? {
        db.execute_batch(RETENTION_FK_INDEX_SCHEMA_V8)?;
    }
    if !has_revision_header_cover(db)? {
        db.execute_batch(REVISION_HEADER_INDEX_SCHEMA_V8)?;
    }
    if !has_revision_document_path_index(db)? {
        db.execute_batch(REVISION_DOCUMENT_PATH_INDEX_SCHEMA_V8)?;
    }
    if missing_supersessions {
        db.execute(
            "INSERT INTO native_revision_supersessions(revision_id,superseded_at,state)
             SELECT r.id,?1,CASE WHEN EXISTS (SELECT 1 FROM revision_capture_inputs c
               WHERE c.revision_id=r.id AND c.input_key='__released:v1')
               THEN 'released' ELSE 'retained' END
             FROM native_revisions r CROSS JOIN index_metadata m
             WHERE m.singleton=1 AND r.id != 'pin:v1:'||m.index_generation||':'||m.index_revision",
            [now],
        )?;
    } else if missing_retention {
        // Earlier v8 could have released a pin before the state column existed.
        // Preserve its tombstone instead of treating it as retained corruption.
        db.execute_batch(
            "UPDATE native_revision_supersessions SET state='released'
             WHERE EXISTS (SELECT 1 FROM revision_capture_inputs c
               WHERE c.revision_id=native_revision_supersessions.revision_id
                 AND c.input_key='__released:v1');",
        )?;
        db.execute(
            "INSERT INTO native_revision_supersessions(revision_id,superseded_at,state)
             SELECT r.id,?1,'released' FROM native_revisions r CROSS JOIN index_metadata m
             WHERE m.singleton=1 AND r.id != 'pin:v1:'||m.index_generation||':'||m.index_revision
               AND EXISTS (SELECT 1 FROM revision_capture_inputs c
                 WHERE c.revision_id=r.id AND c.input_key='__released:v1')
               AND NOT EXISTS (SELECT 1 FROM native_revision_supersessions s WHERE s.revision_id=r.id)",
            [now],
        )?;
    }
    validate_supersessions(db)
}

fn validate_supersessions(db: &Connection) -> Result<()> {
    if !has_revision_supersessions(db)? {
        return Ok(());
    }
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

fn has_revision_producer_bindings(db: &Connection) -> Result<bool> {
    Ok(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='native_binding_epoch')",
        [], |r| r.get(0),
    )?)
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
    let flags = if writable {
        OpenFlags::SQLITE_OPEN_READ_WRITE
    } else {
        OpenFlags::SQLITE_OPEN_READ_ONLY
    } | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let db = protected_sqlite_open(path, flags)?;
    // Inside a read scope a read-only connection's waits share the scope's one
    // deadline (#142); otherwise each SQLite call waits up to `busy_wait`.
    if !writable && read_deadline().is_some() && !busy_wait.is_zero() {
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
        // One selected document's attestation reads the same graph and native
        // pages several times (row budgets, byte budgets, then decoding). A
        // page cache that holds a document's working set serves those repeats
        // from memory. Negative is KiB; pages are allocated only as used, and
        // the cache is freed with the per-request connection. No mmap: an
        // mmap'd I/O error would be a signal, not a typed error.
        storage_result(db.pragma_update(None, "cache_size", READ_CACHE_KIB))?;
        // Attestation's per-document statements stay prepared for the request.
        db.set_prepared_statement_cache_capacity(64);
    }
    let mode: String = storage_result(db.pragma_query_value(None, "journal_mode", |r| r.get(0)))?;
    ensure!(mode == "delete", "incompatible_index: journal mode");
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
    let row = storage_result(rows.next())?
        .context("incompatible_index: missing index metadata marker")?;
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
    let marker = read_index_format_marker(&db)?;
    ensure!(
        marker.schema_version <= i64::from(DATABASE_SCHEMA_VERSION),
        "incompatible_index: unsupported newer schema cannot be rebaselined"
    );
    if marker.is_obsolete() {
        return Err(ObsoleteIndexFormat(marker).into());
    }
    control_ensure!(
        version == DATABASE_SCHEMA_VERSION,
        "incompatible_index: unsupported disposable schema version"
    );
    storage_result(db.prepare(
        "SELECT index_generation,index_revision,indexed_at,stats,diagnostics FROM index_metadata",
    ))?;
    storage_result(db.prepare("SELECT revision_id,source_set_id,language,path,document_version_id,graph_projection_id,class_projection_id FROM revision_documents"))?;
    storage_result(db.prepare(
        "SELECT id,source_set_id,language,path,content_hash,source_bytes FROM document_versions",
    ))?;
    storage_result(db.prepare(
        "SELECT id,document_version_id,language,class_extraction_state FROM graph_projections",
    ))?;
    storage_result(db.prepare("SELECT projection_id,id,payload FROM graph_nodes"))?;
    storage_result(db.prepare("SELECT projection_id,id,payload FROM graph_calls"))?;
    storage_result(db.prepare("SELECT projection_id,id,payload FROM graph_regions"))?;
    storage_result(db.prepare("SELECT id,graph_projection_id FROM class_projections"))?;
    storage_result(db.prepare("SELECT projection_id,id,payload FROM classes"))?;
    storage_result(db.prepare("SELECT projection_id,id,payload FROM class_relations"))?;
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
        || error.downcast_ref::<serde_json::Error>().is_some()
        || error.downcast_ref::<std::string::FromUtf8Error>().is_some()
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

fn selected_integrity(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<OptionalHintExpired>().is_some() {
        return error;
    }
    if recovery_class(&error) != RecoveryClass::Hard
        || error.downcast_ref::<rusqlite::Error>().is_some()
        || error.downcast_ref::<SqliteContention>().is_some()
        || error.downcast_ref::<std::io::Error>().is_some()
    {
        error
    } else {
        SelectedIntegrity(format!("{error:#}")).into()
    }
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
mod terminal_status_contention_tests;

/// Publication contention only. A cached-claim mismatch or unresolved
/// completion also uses the `storage_busy` prefix, but is not a retryable
/// SQLite/lock conflict and must never be silently requeued.
pub(crate) fn transient_storage_contention(error: &anyhow::Error) -> bool {
    if error.chain().any(|cause| {
        cause.downcast_ref::<topology::StorageBusy>().is_some()
            || cause.downcast_ref::<SqliteContention>().is_some()
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
            Err(SqliteContention(()).into())
        }
        other => Ok(other?),
    }
}
#[cfg(test)]
mod sqlite_contention_type_tests;

fn json<T: Serialize + ?Sized>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}
fn rows_at<T: DeserializeOwned>(db: &Connection, sql: &str, revision_id: &str) -> Result<Vec<T>> {
    let mut stmt = storage_result(db.prepare(sql))?;
    let values = storage_result(stmt.query_map([revision_id], |r| r.get::<_, String>(0)))?;
    values
        .map(|v| Ok(serde_json::from_str(&storage_result(v)?)?))
        .collect()
}
fn one_at<T: DeserializeOwned>(
    db: &Connection,
    sql: &str,
    id: &str,
    revision_id: &str,
) -> Result<Option<T>> {
    let value: Option<String> = storage_result(
        db.query_row(sql, params![id, revision_id], |r| r.get(0))
            .optional(),
    )?;
    value.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
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
    ensure!(
        (0..=256 * 1024).contains(&length),
        "incompatible_index: class catalog byte budget exceeded"
    );
    let (warnings, mut truncated): (String, bool) = db.query_row(
        "SELECT class_warnings,class_truncated FROM native_revisions WHERE id=?1",
        [&selected.key],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let warnings: Vec<String> = serde_json::from_str(&warnings)?;
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
) -> Result<Option<(crate::classes::ClassDefinition, usize, bool)>> {
    use crate::class_diagram::CLASS_BYTES;
    let size: Option<i64> = db
        .query_row(
            "SELECT length(CAST(payload AS BLOB)) FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.class_projection_id=classes.projection_id WHERE classes.id=?1",
            params![id, selected.key],
            |r| r.get(0),
        )
        .optional()
        .map_err(|error| store.report_selected_failure(error.into()))?;
    let Some(size) = size else {
        return Ok(None);
    };
    let valid: bool = db
        .query_row(
            "SELECT json_valid(payload) FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.class_projection_id=classes.projection_id WHERE classes.id=?1",
            params![id, selected.key],
            |r| r.get(0),
        )
        .map_err(|error| store.report_selected_failure(error.into()))?;
    if !valid {
        return Err(store.report_selected_failure(
            SelectedIntegrity("incompatible_index: selected class JSON invalid".into()).into(),
        ));
    }
    if size <= CLASS_BYTES as i64 {
        let payload: String = db
            .query_row("SELECT payload FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.class_projection_id=classes.projection_id WHERE classes.id=?1", params![id, selected.key], |r| {
                r.get(0)
            })
            .map_err(|error| store.report_selected_failure(error.into()))?;
        let class = serde_json::from_str(&payload)
            .map_err(|error| store.report_selected_failure(error.into()))?;
        return Ok(Some((class, payload.len(), false)));
    }
    let clip_shape: bool = db
        .query_row(
            "SELECT json_type(payload,'$.fields')='array'
                AND json_type(payload,'$.methods')='array'
                AND NOT EXISTS(SELECT 1 FROM json_each(payload,'$.fields') WHERE type!='object')
                AND NOT EXISTS(SELECT 1 FROM json_each(payload,'$.methods') WHERE type!='object')
             FROM classes JOIN revision_documents m ON m.revision_id=?2 AND m.class_projection_id=classes.projection_id WHERE classes.id=?1",
            params![id, selected.key],
            |r| r.get(0),
        )
        .map_err(|error| store.report_selected_failure(error.into()))?;
    if !clip_shape {
        return Err(store.report_selected_failure(
            SelectedIntegrity("incompatible_index: selected class member JSON invalid".into())
                .into(),
        ));
    }
    for count in [32, 16, 8, 4, 2, 1, 0] {
        let payload: Option<String> = db.query_row("WITH clipped AS (
            SELECT json_set(payload,
                '$.fields',(SELECT json_group_array(json(value)) FROM json_each(classes.payload,'$.fields') WHERE key < ?2),
                '$.methods',(SELECT json_group_array(json(value)) FROM json_each(classes.payload,'$.methods') WHERE key < ?2),
                '$.truncated',json('true')) AS payload FROM classes JOIN revision_documents m ON m.revision_id=?4 AND m.class_projection_id=classes.projection_id WHERE classes.id=?1)
            SELECT CASE WHEN length(CAST(payload AS BLOB)) <= ?3 THEN payload ELSE NULL END FROM clipped",
            params![id, count, CLASS_BYTES as i64, selected.key], |r| r.get(0))
            .map_err(|error| store.report_selected_failure(error.into()))?;
        if let Some(payload) = payload {
            let class = serde_json::from_str(&payload)
                .map_err(|error| store.report_selected_failure(error.into()))?;
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
    for depth in 0..64 {
        ensure!(
            seen.insert(current.clone()),
            InvalidRequest("The selected symbol has a cyclic class ancestry.")
        );
        if db.query_row(
            "SELECT EXISTS(SELECT 1 FROM classes c JOIN revision_documents m ON m.revision_id=?2 AND m.class_projection_id=c.projection_id WHERE c.id=?1)",
            params![current, selected.key],
            |r| r.get::<_, bool>(0),
        )? {
            return Ok(current);
        }
        let symbol = one_at::<Symbol>(db, "SELECT n.payload FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id WHERE n.id=?1", &current, &selected.key)
            .map_err(|error| store.report_selected_failure(error))?
            .ok_or(InvalidRequest(
                "Choose an indexed Java or Python class or method.",
            ))?;
        ensure!(
            depth > 0 || matches!(symbol.kind, SymbolKind::Method | SymbolKind::Function),
            InvalidRequest("Choose an indexed Java or Python class or method.")
        );
        current = symbol.parent.ok_or(InvalidRequest(
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
) -> Result<(crate::classes::ClassDefinition, bool)> {
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
    classes: Vec<&'a crate::classes::ClassDefinition>,
    relations: Vec<&'a crate::classes::ClassRelation>,
    native_declarations: Vec<&'a crate::native_evidence::Declaration>,
    native_calls: Vec<&'a crate::native_evidence::Call>,
    native_regions: Vec<&'a crate::native_evidence::ControlRegion>,
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
        &grouped
            .native_declarations
            .iter()
            .map(|d| (*d).clone())
            .collect::<Vec<_>>(),
        &grouped
            .native_calls
            .iter()
            .map(|c| (*c).clone())
            .collect::<Vec<_>>(),
        &grouped
            .native_regions
            .iter()
            .map(|r| (*r).clone())
            .collect::<Vec<_>>(),
    )
}
fn publication_rows<'a>(
    graph: &'a Graph,
    native: &'a crate::native_evidence::Artifact,
    classes: &'a crate::classes::Catalog,
) -> Result<BTreeMap<&'a str, PublicationRows<'a>>> {
    publication_rows_with_progress(graph, native, classes, None)
}
fn publication_rows_with_progress<'a>(
    graph: &'a Graph,
    native: &'a crate::native_evidence::Artifact,
    classes: &'a crate::classes::Catalog,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<BTreeMap<&'a str, PublicationRows<'a>>> {
    optional_hint_check(progress)?;
    let mut rows = BTreeMap::<&str, PublicationRows>::new();
    for file in &graph.files {
        optional_hint_check(progress)?;
        ensure!(
            rows.insert(&file.path, PublicationRows::default())
                .is_none(),
            "duplicate graph path"
        );
    }
    for node in &graph.nodes {
        optional_hint_check(progress)?;
        rows.get_mut(node.path.as_str())
            .context("graph node path missing")?
            .nodes
            .push(node);
    }
    for call in &graph.calls {
        optional_hint_check(progress)?;
        rows.get_mut(call.path.as_str())
            .context("graph call path missing")?
            .calls
            .push(call);
    }
    for region in &graph.regions {
        optional_hint_check(progress)?;
        rows.get_mut(region.path.as_str())
            .context("graph region path missing")?
            .regions
            .push(region);
    }
    let mut owners = BTreeMap::new();
    for class in &classes.classes {
        optional_hint_check(progress)?;
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
        optional_hint_check(progress)?;
        let path = owners
            .get(relation.owner.as_str())
            .context("class relation owner missing")?;
        rows.get_mut(path)
            .context("class relation path missing")?
            .relations
            .push(relation);
    }
    for decl in &native.declarations {
        optional_hint_check(progress)?;
        rows.get_mut(decl.document.path.as_str())
            .context("native declaration path missing")?
            .native_declarations
            .push(decl);
    }
    for call in &native.calls {
        optional_hint_check(progress)?;
        rows.get_mut(call.document.path.as_str())
            .context("native call path missing")?
            .native_calls
            .push(call);
    }
    for region in &native.control_regions {
        optional_hint_check(progress)?;
        rows.get_mut(region.document.path.as_str())
            .context("native control path missing")?
            .native_regions
            .push(region);
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

fn v8_document_projection(
    file: &SourceFile,
    native: &crate::native_evidence::Artifact,
    grouped: &PublicationRows<'_>,
) -> Result<V8DocumentProjection> {
    v8_document_projection_with_progress(file, native, grouped, None)
}
// Optional hint attestation checks its budget between phases; required
// publication passes no progress and computes the same IDs.
fn v8_document_projection_with_progress(
    file: &SourceFile,
    native: &crate::native_evidence::Artifact,
    grouped: &PublicationRows<'_>,
    progress: Option<&dyn Fn() -> bool>,
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
    optional_hint_check(progress)?;
    let mut nodes = grouped.nodes.clone();
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    let mut calls = grouped.calls.clone();
    calls.sort_by(|a, b| a.id.cmp(&b.id));
    let mut regions = grouped.regions.clone();
    regions.sort_by(|a, b| a.id.cmp(&b.id));
    optional_hint_check(progress)?;
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
    optional_hint_check(progress)?;
    let mut selected_classes = grouped.classes.clone();
    selected_classes.sort_by(|a, b| a.symbol.id.cmp(&b.symbol.id));
    let mut selected_relations = grouped.relations.clone();
    selected_relations.sort_by(|a, b| a.id.cmp(&b.id));
    optional_hint_check(progress)?;
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
    optional_hint_check(progress)?;
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
    pub(crate) changed_path: String,
    pub(crate) native: crate::native_evidence::Artifact,
    prior_pin: IndexPin,
    prior_ids: BTreeMap<String, V8DocumentProjection>,
    prior_coverage: BTreeMap<String, crate::native_evidence::Coverage>,
    prior_extractions: BTreeMap<String, crate::classes::FileExtraction>,
    old_stats: IndexStats,
    old_diagnostics: Vec<Diagnostic>,
    old_class_warnings: Vec<String>,
    old_class_truncated: bool,
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
            .context("missing immutable table")?;
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
                    | "graph_calls"
                    | "graph_regions"
                    | "classes"
                    | "class_relations"
                    | "native_version_coverage_roles"
                    | "native_version_declarations"
                    | "native_version_declaration_ancestors"
                    | "native_version_own_signature_types"
                    | "native_version_ancestor_signature_types"
                    | "native_version_headers"
                    | "native_version_header_items"
                    | "native_version_parameters"
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
            | "graph_calls"
            | "graph_regions"
            | "classes"
            | "class_relations"
            | "native_version_coverage_roles"
            | "native_version_declarations"
            | "native_version_declaration_ancestors"
            | "native_version_own_signature_types"
            | "native_version_ancestor_signature_types"
            | "native_version_headers"
            | "native_version_header_items"
            | "native_version_parameters"
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
            let keys = columns
                .iter()
                .enumerate()
                .filter(|(_, (_, order))| *order > 0)
                .map(|(i, _)| i)
                .collect::<Vec<_>>();
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
            "native_source_sets" | "document_versions" | "graph_projections"
            | "class_projections" => Some(Self::key(db, values, 0)?),
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
                    ("native_version_coverage_roles", "version_id"),
                    ("native_version_declarations", "version_id"),
                    ("native_version_declaration_ancestors", "version_id"),
                    ("native_version_own_signature_types", "version_id"),
                    ("native_version_ancestor_signature_types", "version_id"),
                    ("native_version_headers", "version_id"),
                    ("native_version_header_items", "version_id"),
                    ("native_version_parameters", "version_id"),
                    ("native_version_calls", "version_id"),
                    ("native_version_control_regions", "version_id"),
                    ("native_version_call_regions", "version_id"),
                ],
            ),
            (
                "graph_projections",
                &[
                    ("graph_nodes", "projection_id"),
                    ("graph_calls", "projection_id"),
                    ("graph_regions", "projection_id"),
                ],
            ),
            (
                "class_projections",
                &[
                    ("classes", "projection_id"),
                    ("class_relations", "projection_id"),
                ],
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

// Emit exactly one document's graph and class rows under the caller's transaction.
fn write_graph_document(
    db: &Connection,
    grouped: &PublicationRows<'_>,
    ids: &V8DocumentProjection,
    reused: ReusedFamilies,
    immutable: &mut ImmutableAppend,
) -> Result<()> {
    for n in &grouped.nodes {
        if reused.graph {
            continue;
        }
        immutable.insert(
            db,
            "INSERT INTO graph_nodes VALUES(?1,?2,?3,?4,?5)",
            params![ids.graph_id, n.id, n.name, n.path, json(n)?],
        )?;
    }
    for c in &grouped.calls {
        if reused.graph {
            continue;
        }
        immutable.insert(
            db,
            "INSERT INTO graph_calls VALUES(?1,?2,?3,?4,?5,?6)",
            params![
                ids.graph_id,
                c.id,
                c.caller,
                Option::<String>::None,
                c.path,
                json(c)?
            ],
        )?;
    }
    for r in &grouped.regions {
        if reused.graph {
            continue;
        }
        immutable.insert(
            db,
            "INSERT INTO graph_regions VALUES(?1,?2,?3,?4,?5)",
            params![ids.graph_id, r.id, r.owner, r.path, json(r)?],
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
                ids.class_id,
                ids.graph_id,
                class.symbol.id,
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
                ids.class_id,
                relation.id,
                relation.owner,
                relation.target,
                json(relation)?,
            ],
        )?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static WRITER_SOURCE_SQL_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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

#[allow(clippy::too_many_arguments)] // The transaction seam keeps the immutable bundle and reuse witness separate.
fn write_native(
    db: &Connection,
    bundle: (
        &crate::native_evidence::Artifact,
        &crate::capture::Capture,
        &Graph,
        &crate::classes::Catalog,
        &BTreeMap<String, crate::classes::FileExtraction>,
    ),
    revision: IndexPin,
    stats: &IndexStats,
    cancel: &CancelFlag,
    immutable: &mut ImmutableAppend,
    reuse: &PreflightReuse,
    rows: &BTreeMap<&str, PublicationRows<'_>>,
    full_diagnostics: &[Diagnostic],
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
            "INSERT INTO native_revisions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                revision_key,
                a.revision.source_set_id,
                a.revision.toolchain_hash,
                a.revision.config_hash,
                a.revision.dependency_hash,
                a.revision.id,
                json(&a.revision.documents)?,
                json(&a.source_set.dependencies)?,
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
                "INSERT INTO document_versions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                params![
                    ids.version_id,
                    a.source_set.id,
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
        if !reuse.for_path(&file.path).graph {
            immutable.insert(
                db,
                "INSERT INTO graph_projections VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    ids.graph_id,
                    ids.version_id,
                    file.language,
                    ids.graph_hash,
                    "ready",
                    if matches!(file.language.as_str(), "java" | "python") {
                        "ready"
                    } else {
                        "notApplicable"
                    },
                    extractions.get(&file.path).map(json).transpose()?,
                ],
            )?;
        }
        if !reuse.for_path(&file.path).class {
            immutable.insert(
                db,
                "INSERT INTO class_projections VALUES(?1,?2,?3,?4)",
                params![ids.class_id, ids.graph_id, ids.class_hash, "ready"],
            )?;
        }
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
                a.source_set.id,
                file.language,
                file.path,
                ids.version_id,
                ids.graph_id,
                ids.class_id,
                json(&capture.source_stat(&file.path)?)?,
                coverage.requested,
                coverage.selected,
                coverage.state,
                coverage.diagnostic,
                ordinal as i64,
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
                    params![ids.version_id, kind, role, ordinal as i64],
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
        let version = &projections
            .get(&d.document.path)
            .context("native declaration missing document")?
            .version_id;
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
            version,d.syntax_id,owner,d.kind,d.role.as_str(),d.name,d.lookup_key,sig.is_some(),sig.map(|s|s.type_parameter_count as i64),
            sig.map(|s|s.variadic),d.key.ordinal as i64,d.range.start as i64,d.range.end as i64,
            d.name_range.as_ref().map(|r|r.start as i64),d.name_range.as_ref().map(|r|r.end as i64),
        ])?;
        if let Some(sig) = sig {
            for (ordinal, parameter) in sig.parameter_types.iter().enumerate() {
                immutable.insert(
                    db,
                    "INSERT INTO native_version_own_signature_types VALUES(?1,?2,?3,?4)",
                    params![version, d.syntax_id, ordinal as i64, parameter],
                )?;
            }
        }
        for (ordinal, key) in d.ancestors.iter().enumerate() {
            let sig = key.signature.as_ref();
            immutable.insert(db,"INSERT INTO native_version_declaration_ancestors VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![
                version,d.syntax_id,ordinal as i64,key.kind,key.name,key.ordinal as i64,sig.is_some(),
                sig.map(|s|s.type_parameter_count as i64),sig.map(|s|s.variadic),
            ])?;
            if let Some(sig) = sig {
                for (parameter_ordinal, parameter) in sig.parameter_types.iter().enumerate() {
                    immutable.insert(db,"INSERT INTO native_version_ancestor_signature_types VALUES(?1,?2,?3,?4,?5)",params![
                        version,d.syntax_id,ordinal as i64,parameter_ordinal as i64,parameter,
                    ])?;
                }
            }
        }
        immutable.insert(
            db,
            "INSERT INTO native_version_headers VALUES(?1,?2,?3,?4,?5)",
            params![
                version,
                d.syntax_id,
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
                    params![version, d.syntax_id, kind, ordinal as i64, item],
                )?;
            }
        }
        for (ordinal, parameter) in d.header.parameters.iter().enumerate() {
            immutable.insert(
                db,
                "INSERT INTO native_version_parameters VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    version,
                    d.syntax_id,
                    ordinal as i64,
                    parameter.name,
                    parameter.type_name,
                    parameter.variadic,
                ],
            )?;
        }
    }
    for r in &a.control_regions {
        check_cancel(cancel)?;
        if reuse.for_path(&r.document.path).native {
            continue;
        }
        let version = &projections
            .get(&r.document.path)
            .context("native region missing document")?
            .version_id;
        immutable.insert(
            db,
            "INSERT INTO native_version_control_regions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                version,
                r.id,
                r.owner_syntax_id,
                r.ordinal as i64,
                r.kind,
                r.range.start as i64,
                r.range.end as i64,
                r.parent_id,
                r.arm,
            ],
        )?;
    }
    let documents_by_key: BTreeMap<_, _> = selected_documents
        .iter()
        .map(|document| (&document.key, document))
        .collect();
    for c in &a.calls {
        check_cancel(cancel)?;
        if reuse.for_path(&c.document.path).native {
            continue;
        }
        let version = &projections
            .get(&c.document.path)
            .context("native call missing document")?
            .version_id;
        let document = documents_by_key
            .get(&c.document)
            .copied()
            .context("native call missing validated document")?;
        // A new streamed version was inserted above from this exact admitted
        // file. Avoid one SQLite source BLOB read per native call. The public
        // plain-artifact path still checks stored source bytes as before;
        // reused native versions skip this loop entirely.
        let stored_source: Option<Vec<u8>> = if captured_file.is_none() {
            #[cfg(test)]
            WRITER_SOURCE_SQL_READS.with(|reads| reads.set(reads.get() + 1));
            Some(db.query_row(
                "SELECT source_bytes FROM document_versions WHERE id=?1 AND source_set_id=?2 AND language=?3 AND path=?4",
                params![version, document.key.source_set_id, document.key.language, document.key.path],
                |row| row.get(0),
            )?)
        } else {
            None
        };
        let source = if let Some(file) = captured_file {
            file.text.as_str()
        } else {
            std::str::from_utf8(
                stored_source
                    .as_deref()
                    .context("native stored source missing")?,
            )
            .context("native stored source is not UTF-8")?
        };
        let callee_lookup_key =
            crate::native_evidence::verified_callee_lookup(&document.key.language, c, source);
        immutable.insert(
            db,
            "INSERT INTO native_version_calls VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                version,
                c.id,
                c.owner_syntax_id,
                c.ordinal as i64,
                c.range.start as i64,
                c.range.end as i64,
                c.callee_range.as_ref().map(|r| r.start as i64),
                c.callee_range.as_ref().map(|r| r.end as i64),
                c.spelling,
                document.key.source_set_id,
                document.key.language,
                callee_lookup_key,
            ],
        )?;
        for (ordinal, region) in c.region_ids.iter().enumerate() {
            immutable.insert(
                db,
                "INSERT INTO native_version_call_regions VALUES(?1,?2,?3,?4)",
                params![version, c.id, region, ordinal as i64],
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
    // Fixed identifiers from CACHE_SCHEMA_V8 only. Do not read table names from
    // sqlite_master or treat a partial publication as a valid empty bootstrap.
    for name in [
        "class_projections",
        "class_relations",
        "classes",
        "document_versions",
        "graph_calls",
        "graph_nodes",
        "graph_projections",
        "graph_regions",
        "native_producer_inputs",
        "native_producer_languages",
        "native_producers",
        "native_revisions",
        "native_source_set_dependencies",
        "native_source_set_languages",
        "native_source_sets",
        "native_version_ancestor_signature_types",
        "native_version_call_regions",
        "native_version_calls",
        "native_version_control_regions",
        "native_version_coverage_roles",
        "native_version_declaration_ancestors",
        "native_version_declarations",
        "native_version_header_items",
        "native_version_headers",
        "native_version_own_signature_types",
        "native_version_parameters",
        "revision_capture_inputs",
        "revision_documents",
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
    ensure!(
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
    ensure!(
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
        ensure!(
            key.len() <= 8192 && raw.len() <= 8192,
            "incompatible_index: selected producer input byte budget exceeded"
        );
        let observation: crate::capture::CaptureInputObservation = serde_json::from_str(&raw)?;
        ensure!(
            json(&observation)? == raw,
            "incompatible_index: noncanonical producer input"
        );
        if let Some(relative) = key.strip_prefix("toolchain:") {
            ensure!(
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
                anyhow::bail!("incompatible_index: selected executable input absent");
            };
            ensure!(
                Path::new(path).is_absolute()
                    && hash.len() == 64
                    && hash
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    && executable.replace(hash.clone()).is_none(),
                "incompatible_index: selected executable input differs"
            );
        } else {
            anyhow::bail!("incompatible_index: selected producer input role differs");
        }
        identities.push(serde_json::json!({"key":key,"observation":observation}));
    }
    ensure!(
        selectors.len() == crate::capture::ROOT_INPUTS[22..].len()
            && identities.len() == selectors.len() + 1,
        "incompatible_index: selected producer input inventory incomplete"
    );
    let executable = executable.context("incompatible_index: selected executable input missing")?;
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
    if !has_revision_producer_bindings(db)? {
        ensure!(
            executable == origin,
            "incompatible_index: legacy selected producer differs from origin"
        );
        return Ok(executable);
    }
    let epoch: Vec<(String, i64)> = db
        .prepare("SELECT index_generation,first_revision FROM native_binding_epoch LIMIT 2")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let head_revision: i64 = db.query_row(
        "SELECT index_revision FROM index_metadata WHERE singleton=1",
        [],
        |r| r.get(0),
    )?;
    ensure!(
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
        ensure!(
            executable == origin && persisted.is_none(),
            "incompatible_index: legacy selected producer provenance differs"
        );
    } else {
        let (producer_sha, binding_sha) =
            persisted.context("incompatible_index: selected revision producer binding missing")?;
        ensure!(
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

// One header query works before and after the additive v8 index is installed.
// Once present, SQLite reads the narrow covering index instead of wide JSON rows.
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
    // The planner uses the narrow covering index once leader startup installs it.
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
    if !has_revision_release_debt(db)? {
        // A prior v8 index has no maintenance extension yet. Validate its
        // complete retained manifests before the first atomic installation.
        // A release leaves its header as a durable tombstone. Missing manifests
        // without that exact marker (including an empty legitimate revision) are
        // never accepted as released. The marker owns no native capture input.
        let malformed: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM native_revisions r WHERE
                (EXISTS(SELECT 1 FROM revision_capture_inputs i
                    WHERE i.revision_id=r.id AND i.input_key='__released:v1') AND
                  ((SELECT count(*) FROM revision_capture_inputs i WHERE i.revision_id=r.id)!=1 OR
                   NOT EXISTS(SELECT 1 FROM revision_capture_inputs i
                     WHERE i.revision_id=r.id AND i.input_key='__released:v1' AND i.payload='released:v1') OR
                   EXISTS(SELECT 1 FROM revision_documents d WHERE d.revision_id=r.id)))
                OR (NOT EXISTS(SELECT 1 FROM revision_capture_inputs i
                    WHERE i.revision_id=r.id AND i.input_key='__released:v1') AND
                   (SELECT count(*) FROM revision_documents d WHERE d.revision_id=r.id)
                       != json_array_length(r.source_inventory)))",
            [], |row| row.get(0),
        )?;
        control_ensure!(
            !malformed,
            "incompatible_index: retained manifest/release mismatch"
        );
    } else {
        // Pending retains original inputs while its manifest may be partly drained.
        // A changed predecessor has just gained its supersession row; any pending
        // debt is also mutable. Only Store admission/replacement sweeps all history.
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
    }
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
    let _: IndexStats = serde_json::from_str(&header_stats)?;
    let _: Vec<Diagnostic> = serde_json::from_str(&header_diagnostics)?;
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
const SELECTED_CAPTURE_SNAPSHOT_SQL: &str = "SELECT d.path,v.content_hash,d.capture_stat FROM revision_documents d JOIN document_versions v ON v.id=d.document_version_id JOIN native_revisions r ON r.id=d.revision_id JOIN index_metadata m ON m.index_revision=r.published_index_revision AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision WHERE d.revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) ORDER BY d.path";

fn compare_capture_snapshot(
    db: &Connection,
    capture: &crate::capture::Capture,
) -> Result<ScanComparison> {
    compare_capture_snapshot_with(db, capture, true)
}

/// The selected-reuse comparison: the executable observation is left out, so
/// an executable rebuild alone is no change. Native reuse is decided by the
/// recorded extractor producer hash, which is compared (Decision 0005).
fn compare_capture_snapshot_for_reuse(
    db: &Connection,
    capture: &crate::capture::Capture,
) -> Result<ScanComparison> {
    compare_capture_snapshot_with(db, capture, false)
}

fn compare_capture_snapshot_with(
    db: &Connection,
    capture: &crate::capture::Capture,
    include_executable: bool,
) -> Result<ScanComparison> {
    let mut previous_sources = BTreeMap::new();
    let mut statement = db.prepare(SELECTED_CAPTURE_SNAPSHOT_SQL)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (path, hash, stat) = row?;
        let stat: crate::capture::CaptureStat = serde_json::from_str(&stat)?;
        ensure!(
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
        let observation: crate::capture::CaptureInputObservation = serde_json::from_str(&payload)?;
        ensure!(
            previous_inputs.insert(key, observation).is_none(),
            "incompatible_index: duplicate captured input observation"
        );
    }
    let mut current_inputs = capture.persisted_inputs()?;
    if !include_executable {
        previous_inputs.retain(|key: &String, _| !key.starts_with("executable:"));
        current_inputs.retain(|key, _| !key.starts_with("executable:"));
    }
    Ok(compare_capture_observations(
        &previous_sources,
        &current_sources,
        &previous_inputs,
        &current_inputs,
    ))
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
    let decoded_options: crate::indexer::ReconcileOptions =
        serde_json::from_str(&options).context("incompatible_index: invalid reconcile options")?;
    control_ensure!(
        json(&decoded_options)? == options
            && decoded_options.version == 1
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
        let decoded: crate::capture::CaptureStat =
            serde_json::from_str(&payload).context("incompatible_index: invalid capture stat")?;
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
        let observation: crate::capture::CaptureInputObservation =
            serde_json::from_str(&payload).context("incompatible_index: invalid capture input")?;
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
                    || key.starts_with("presentation-scip:")
                    || key.starts_with("presentation-manifest:")
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
                    || key.starts_with("ignore:")
                    || key.starts_with("presentation-scip:")
                    || key.starts_with("presentation-manifest:") => {}
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
    expected.insert(
        executable.ok_or_else(|| {
            ControlIntegrity("incompatible_index: missing executable input".into())
        })?,
    );
    match &decoded_options.scip_path {
        Some(path) => {
            control_ensure!(!path.is_empty(), "incompatible_index: empty SCIP option");
            expected.insert(format!("presentation-scip:{path}"));
        }
        None => control_ensure!(
            !actual
                .iter()
                .any(|key| key.starts_with("presentation-scip:")),
            "incompatible_index: unconfigured SCIP input"
        ),
    }
    match &decoded_options.manifest_path {
        Some(path) => {
            control_ensure!(
                !path.is_empty(),
                "incompatible_index: empty manifest option"
            );
            expected.insert(format!("presentation-manifest:{path}"));
        }
        None => control_ensure!(
            !actual
                .iter()
                .any(|key| key.starts_with("presentation-manifest:")),
            "incompatible_index: unconfigured manifest input"
        ),
    }
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
    let _: IndexStats = serde_json::from_str(&stats)?;
    let _: Vec<Diagnostic> = serde_json::from_str(&diagnostics)?;
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
    ensure!(
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
    ensure!(
        count("native_producers")? == 1
            && count("native_source_sets")? == 1
            && count("native_revisions")? >= 1,
        "incompatible_index: missing native pair"
    );
    let (manifest_rows, projection_rows, inventory_rows): (i64, i64, i64) = db.query_row(
        "SELECT (SELECT count(*) FROM revision_documents d WHERE d.revision_id=r.id),
                (SELECT count(*) FROM revision_documents d WHERE d.revision_id=r.id AND d.class_projection_id IS NOT NULL),
                json_array_length(r.source_inventory)
         FROM native_revisions r JOIN index_metadata m ON m.singleton=1
           AND r.published_index_revision=m.index_revision
           AND r.id='pin:v1:'||m.index_generation||':'||m.index_revision",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    ensure!(
        manifest_rows == inventory_rows && projection_rows == manifest_rows,
        "incompatible_index: incomplete revision manifest"
    );
    let mut stmt=db.prepare("SELECT d.source_bytes,d.content_hash,d.byte_length,r.path,r.language,r.source_set_id,g.language
        FROM revision_documents r JOIN document_versions d ON d.id=r.document_version_id
        JOIN graph_projections g ON g.id=r.graph_projection_id")?;
    let mut matched = 0;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        use sha2::{Digest, Sha256};
        let bytes: Vec<u8> = row.get(0)?;
        let hash: String = row.get(1)?;
        let length: i64 = row.get(2)?;
        let _ = String::from_utf8(bytes.clone())?;
        ensure!(
            length == bytes.len() as i64
                && hash == hex::encode(Sha256::digest(&bytes))
                && row.get::<_, String>(4)? == row.get::<_, String>(6)?
                && row.get::<_, String>(5)?.starts_with("source-set:v1:")
                && !row.get::<_, String>(3)?.is_empty(),
            "incompatible_index: manifest source mismatch"
        );
        matched += 1;
    }
    ensure!(
        matched == count("revision_documents")?,
        "incompatible_index: missing manifest document"
    );
    ensure!(
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
fn v8_native_scope_for(
    db: &Connection,
    key: &crate::native_evidence::DocumentKey,
    selected: &ReadRevision,
) -> Result<Option<V8NativeScope>> {
    let mut stmt = db.prepare(
        "SELECT rd.revision_id,rd.document_version_id,r.native_revision_id,d.producer_id
        FROM native_revisions r JOIN revision_documents rd ON rd.revision_id=r.id
        JOIN document_versions d ON d.id=rd.document_version_id
          AND d.source_set_id=rd.source_set_id AND d.language=rd.language AND d.path=rd.path
        WHERE r.id=?4 AND rd.source_set_id=?1 AND rd.language=?2 AND rd.path=?3 LIMIT 2",
    )?;
    let rows = stmt
        .query_map(
            params![key.source_set_id, key.language, key.path, selected.key],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    ensure!(
        rows.len() <= 1,
        "incompatible_index: duplicate selected document manifest"
    );
    Ok(rows.into_iter().next().map(
        |(revision_key, version_id, native_revision_id, producer_id)| V8NativeScope {
            revision_key,
            version_id,
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
    let mut stmt = db.prepare(
        "SELECT m.source_set_id,m.language,m.path
        FROM native_version_declarations d JOIN revision_documents m
          ON m.revision_id=?2 AND m.document_version_id=d.version_id
        WHERE d.syntax_id=?1 LIMIT 2",
    )?;
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
        "incompatible_index: duplicate native owner identity"
    );
    keys.first()
        .map(|key| {
            v8_native_scope_for(db, key, selected)?
                .context("incompatible_index: native owner manifest missing")
        })
        .transpose()
}
fn contiguous(ordinal: i64, expected: usize, what: &str) -> Result<()> {
    ensure!(
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
        ensure!(
            values.is_empty(),
            "incompatible_index: orphan native signature types"
        );
        return Ok(None);
    }
    Ok(Some(crate::native_evidence::Signature {
        parameter_types: values,
        type_parameter_count: usize::try_from(count.context("missing native signature count")?)?,
        variadic: variadic.context("missing native signature variadic")?,
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

fn read_native_declarations_v8_inner(
    db: &Connection,
    scope: &V8NativeScope,
    lookup_key: Option<&str>,
    all_lookup_keys: bool,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<Vec<crate::native_evidence::Declaration>> {
    use crate::native_evidence::{Declaration, Header, Key, Parameter, Range};
    optional_hint_check(progress)?;
    let v = &scope.version_id;
    let mut own_types: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for row in db.prepare("SELECT syntax_id,ordinal,type_name FROM native_version_own_signature_types WHERE version_id=?1 ORDER BY syntax_id,ordinal")?
        .query_map([v],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?)))? {
        optional_hint_check(progress)?;
        let (id,ord,value)=row?;let entry=own_types.entry(id).or_default();
        contiguous(ord,entry.len(),"own signature")?;entry.push(value);
    }
    let mut ancestor_types: BTreeMap<(String, i64), Vec<String>> = BTreeMap::new();
    for row in db.prepare("SELECT syntax_id,ancestor_ordinal,ordinal,type_name FROM native_version_ancestor_signature_types WHERE version_id=?1 ORDER BY syntax_id,ancestor_ordinal,ordinal")?
        .query_map([v],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?)))? {
        optional_hint_check(progress)?;
        let (id,ancestor,ord,value)=row?;let entry=ancestor_types.entry((id,ancestor)).or_default();
        contiguous(ord,entry.len(),"ancestor signature")?;entry.push(value);
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
    for row in db.prepare("SELECT syntax_id,ordinal,kind,name,sibling_ordinal,signature_present,type_parameter_count,variadic FROM native_version_declaration_ancestors WHERE version_id=?1 ORDER BY syntax_id,ordinal")?
        .query_map([v],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,i64>(4)?,r.get::<_,bool>(5)?,r.get::<_,Option<i64>>(6)?,r.get::<_,Option<bool>>(7)?)))? {
        optional_hint_check(progress)?;
        let (id,ord,kind,name,sibling,present,count,variadic)=row?;
        let entry=ancestors.entry(id).or_default();contiguous(ord,entry.len(),"ancestor")?;
        entry.push((ord,kind,name,sibling,present,count,variadic));
    }
    let mut headers: BTreeMap<String, Header> = BTreeMap::new();
    for row in db.prepare("SELECT syntax_id,kind,name,result_type FROM native_version_headers WHERE version_id=?1 ORDER BY syntax_id")?
        .query_map([v],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?)))? {
        optional_hint_check(progress)?;
        let (id,kind,name,result_type)=row?;
        ensure!(headers.insert(id,Header{kind,name,modifiers:vec![],type_parameters:vec![],parameters:vec![],result_type,bases:vec![]}).is_none(),"duplicate native header");
    }
    // Separate ordinal spaces for each (syntax_id,item_kind); don't collapse them.
    let mut item_ordinals: BTreeMap<(String, String), usize> = BTreeMap::new();
    for row in db.prepare("SELECT syntax_id,item_kind,ordinal,value FROM native_version_header_items WHERE version_id=?1 ORDER BY syntax_id,item_kind,ordinal")?
        .query_map([v],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?)))? {
        optional_hint_check(progress)?;
        let (id,kind,ordinal,value)=row?;
        let n=item_ordinals.entry((id.clone(),kind.clone())).or_default();contiguous(ordinal,*n,"header item")?;*n+=1;
        let h=headers.get_mut(&id).context("orphan native header item")?;
        match kind.as_str(){"modifier"=>h.modifiers.push(value),"typeParameter"=>h.type_parameters.push(value),"base"=>h.bases.push(value),_=>anyhow::bail!("unknown header item")}
    }
    let mut param_ordinals: BTreeMap<String, usize> = BTreeMap::new();
    for row in db.prepare("SELECT syntax_id,ordinal,name,type_name,variadic FROM native_version_parameters WHERE version_id=?1 ORDER BY syntax_id,ordinal")?
        .query_map([v],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,Option<String>>(3)?,r.get::<_,bool>(4)?)))? {
        optional_hint_check(progress)?;
        let (id,ordinal,name,type_name,variadic)=row?;
        let n=param_ordinals.entry(id.clone()).or_default();contiguous(ordinal,*n,"header parameter")?;*n+=1;
        headers.get_mut(&id).context("orphan native header parameter")?.parameters.push(Parameter{name,type_name,variadic});
    }
    let proof = scope.proof_id();
    let mut result = Vec::new();
    let mut recorded_owners: BTreeMap<String, Option<String>> = BTreeMap::new();
    for row in db.prepare("SELECT syntax_id,owner_syntax_id,kind,role,name,lookup_key,key_signature_present,key_type_parameter_count,key_variadic,key_ordinal,start_byte,end_byte,name_start,name_end FROM native_version_declarations WHERE version_id=?1 ORDER BY syntax_id")?
      .query_map([v],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,Option<String>>(5)?,r.get::<_,bool>(6)?,r.get::<_,Option<i64>>(7)?,r.get::<_,Option<bool>>(8)?,r.get::<_,i64>(9)?,r.get::<_,i64>(10)?,r.get::<_,i64>(11)?,r.get::<_,Option<i64>>(12)?,r.get::<_,Option<i64>>(13)?)))? {
        optional_hint_check(progress)?;
        let (id,owner,kind,role,name,lookup,present,count,variadic,ordinal,start,end,name_start,name_end)=row?;
        let role = crate::native_evidence::DeclarationRole::from_stored(&role)?;
        ensure!(recorded_owners.insert(id.clone(),owner).is_none(),"duplicate native declaration");
        let key=Key{kind:kind.clone(),name:name.clone(),signature:decode_signature(&mut own_types,&id,present,count,variadic)?,ordinal:usize::try_from(ordinal)?};
        let mut a=Vec::new();
        for (ancestor_ord,kind,name,sibling,present,count,variadic) in ancestors.remove(&id).unwrap_or_default() {
            contiguous(ancestor_ord,a.len(),"ancestor")?;
            let sig_types=ancestor_types.remove(&(id.clone(),ancestor_ord)).unwrap_or_default();
            if !present {ensure!(sig_types.is_empty(),"orphan ancestor signature types");}
            let signature=if present {Some(crate::native_evidence::Signature{parameter_types:sig_types,type_parameter_count:usize::try_from(count.context("missing ancestor signature count")?)?,variadic:variadic.context("missing ancestor signature variadic")?})}else{None};
            a.push(Key{kind,name,signature,ordinal:usize::try_from(sibling)?});
        }
        let name_range=name_start.zip(name_end).map(|(start,end)|->Result<Range>{Ok(Range{start:usize::try_from(start)?,end:usize::try_from(end)?})}).transpose()?;
        ensure!(name_start.is_some()==name_end.is_some(),"half native name range");
        result.push(Declaration{role,syntax_id:id.clone(),document:scope.key.clone(),revision_id:scope.native_revision_id.clone(),kind,name,lookup_key:lookup,ancestors:a,key,
            range:Range{start:usize::try_from(start)?,end:usize::try_from(end)?},name_range,header:headers.remove(&id).context("missing native header")?,provenance_id:proof.clone()});
    }
    ensure!(
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
        ensure!(
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
                    .context("missing native parent")
            })
            .transpose()?;
        ensure!(
            recorded_owners.get(&d.syntax_id) == Some(&expected),
            "native parent differs from ancestor key"
        );
    }
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
        db.query_row("SELECT coverage_requested,coverage_selected,coverage_state,coverage_diagnostic FROM revision_documents WHERE revision_id=?1 AND source_set_id=?2 AND language=?3 AND path=?4 AND document_version_id=?5",
            params![scope.revision_key,scope.key.source_set_id,scope.key.language,scope.key.path,scope.version_id],
            |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
    let (mut supported_roles, mut observed_roles) = (vec![], vec![]);
    for row in db.prepare("SELECT role_kind,role,ordinal FROM native_version_coverage_roles WHERE version_id=?1 ORDER BY role_kind,ordinal")?
        .query_map([&scope.version_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))? {
        let (kind,role,ordinal)=row?;
        let out=match kind.as_str(){"supported"=>&mut supported_roles,"observed"=>&mut observed_roles,_=>anyhow::bail!("invalid native coverage role kind")};
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
fn read_native_calls_v8_inner(
    db: &Connection,
    scope: &V8NativeScope,
    owner: Option<&str>,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<Vec<crate::native_evidence::Call>> {
    optional_hint_check(progress)?;
    use crate::native_evidence::{Call, Range};
    let version = &scope.version_id;
    let mut known = BTreeSet::new();
    for row in db
        .prepare("SELECT syntax_id FROM native_version_declarations WHERE version_id=?1")?
        .query_map([version], |r| r.get::<_, String>(0))?
    {
        optional_hint_check(progress)?;
        known.insert(row?);
    }
    let mut region_owner = BTreeMap::new();
    for row in db
        .prepare(
            "SELECT id,owner_syntax_id FROM native_version_control_regions WHERE version_id=?1",
        )?
        .query_map([version], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?
    {
        optional_hint_check(progress)?;
        let (id, owner) = row?;
        region_owner.insert(id, owner);
    }
    let proof = scope.proof_id();
    let mut calls: Vec<Call> = vec![];
    let mut indexes: BTreeMap<String, usize> = BTreeMap::new();
    let mut ordinals: BTreeMap<String, usize> = BTreeMap::new();
    for row in db.prepare("SELECT id,owner_syntax_id,ordinal,start_byte,end_byte,callee_start,callee_end,spelling,source_set_id,language,callee_lookup_key FROM native_version_calls WHERE version_id=?1 ORDER BY owner_syntax_id,ordinal")?
        .query_map([version],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,Option<i64>>(5)?,r.get::<_,Option<i64>>(6)?,r.get::<_,Option<String>>(7)?,r.get::<_,String>(8)?,r.get::<_,String>(9)?,r.get::<_,Option<String>>(10)?)))? {
        optional_hint_check(progress)?;
        let (id,call_owner,ordinal,start,end,callee_start,callee_end,spelling,source_set_id,language,callee_lookup_key)=row?;
        ensure!(source_set_id == scope.key.source_set_id && language == scope.key.language, "native call document mismatch");
        ensure!(callee_lookup_key.is_none() || callee_start.is_some() && spelling.is_some(), "invalid native call lookup key");
        ensure!(known.contains(&call_owner),"orphan native call owner");
        let n=ordinals.entry(call_owner.clone()).or_default();contiguous(ordinal,*n,"call")?;*n+=1;
        ensure!(callee_start.is_some()==callee_end.is_some(),"half callee range");
        let callee_range=callee_start.zip(callee_end).map(|(start,end)|->Result<Range>{
            Ok(Range{start:usize::try_from(start)?,end:usize::try_from(end)?})
        }).transpose()?;
        ensure!(indexes.insert(id.clone(),calls.len()).is_none(),"duplicate native call ID");
        calls.push(Call{id,owner_syntax_id:call_owner,ordinal:usize::try_from(ordinal)?,document:scope.key.clone(),
            revision_id:scope.native_revision_id.clone(),range:Range{start:usize::try_from(start)?,end:usize::try_from(end)?},
            callee_range,spelling,region_ids:vec![],provenance_id:proof.clone()});
    }
    for row in db.prepare("SELECT call_id,region_id,ordinal FROM native_version_call_regions WHERE version_id=?1 ORDER BY call_id,ordinal")?
        .query_map([version],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?)))? {
        optional_hint_check(progress)?;
        let (call_id,region_id,ordinal)=row?;
        let call=&mut calls[*indexes.get(&call_id).context("orphan native call region")?];
        ensure!(region_owner.get(&region_id)==Some(&call.owner_syntax_id),"cross-owner native call region");
        contiguous(ordinal,call.region_ids.len(),"call region")?;
        call.region_ids.push(region_id);
    }
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
fn read_native_control_regions_v8_inner(
    db: &Connection,
    scope: &V8NativeScope,
    owner: Option<&str>,
    progress: Option<&dyn Fn() -> bool>,
) -> Result<Vec<crate::native_evidence::ControlRegion>> {
    optional_hint_check(progress)?;
    use crate::native_evidence::{ControlRegion, Range};
    let mut known = BTreeSet::new();
    for row in db
        .prepare("SELECT syntax_id FROM native_version_declarations WHERE version_id=?1")?
        .query_map([&scope.version_id], |r| r.get::<_, String>(0))?
    {
        optional_hint_check(progress)?;
        known.insert(row?);
    }
    let proof = scope.proof_id();
    let mut regions = Vec::<ControlRegion>::new();
    let mut indexes = BTreeMap::<String, usize>::new();
    let mut ordinals = BTreeMap::<String, usize>::new();
    for row in db.prepare("SELECT id,owner_syntax_id,ordinal,kind,start_byte,end_byte,parent_id,arm FROM native_version_control_regions WHERE version_id=?1 ORDER BY owner_syntax_id,ordinal")?
        .query_map([&scope.version_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?,r.get::<_,Option<String>>(6)?,r.get::<_,Option<String>>(7)?)))? {
        optional_hint_check(progress)?;
        let (id,region_owner,ordinal,kind,start,end,parent_id,arm)=row?;
        ensure!(known.contains(&region_owner),"orphan native region owner");
        let n=ordinals.entry(region_owner.clone()).or_default();contiguous(ordinal,*n,"region")?;*n+=1;
        ensure!(indexes.insert(id.clone(),regions.len()).is_none(),"duplicate native region ID");
        regions.push(ControlRegion{id,owner_syntax_id:region_owner,ordinal:usize::try_from(ordinal)?,
            document:scope.key.clone(),revision_id:scope.native_revision_id.clone(),kind,
            range:Range{start:usize::try_from(start)?,end:usize::try_from(end)?},parent_id,arm,
            provenance_id:proof.clone()});
    }
    for region in &regions {
        optional_hint_check(progress)?;
        if let Some(parent_id) = &region.parent_id {
            let parent = &regions[*indexes
                .get(parent_id)
                .context("missing native parent region")?];
            ensure!(
                region.owner_syntax_id == parent.owner_syntax_id
                    && parent.range.start <= region.range.start
                    && region.range.end <= parent.range.end,
                "invalid native parent region"
            );
        }
    }
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

fn native_index_unavailable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<topology::IndexNotReady>().is_some()
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

    /// The terminal-commit count to record before re-reading a request row.
    pub fn terminal_generation(&self) -> u64 {
        *self.publication_gate.terminal_commits.lock().unwrap()
    }

    /// Block until a terminal COMMIT after `seen`; then re-read the row.
    pub fn wait_terminal_change(&self, seen: u64) {
        let gate = &self.publication_gate;
        let commits = gate.terminal_commits.lock().unwrap();
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
    /// Inspect only already published, inactive index queues. Unknown paths or
    /// unreadable identities keep the daemon alive rather than abandoning work.
    pub fn orphan_queues_pending(
        roots: &topology::TopologyRoots,
        active: &std::collections::HashSet<String>,
    ) -> bool {
        let parent = roots.cache.join("indexes");
        let dirs = match std::fs::read_dir(&parent) {
            Ok(dirs) => dirs,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return false,
            Err(_) => return true,
        };
        for entry in dirs {
            let Ok(entry) = entry else { return true };
            let Ok(kind) = entry.file_type() else {
                return true;
            };
            if !kind.is_dir() {
                continue;
            }
            let Some(key) = entry.file_name().to_str().map(str::to_owned) else {
                return true;
            };
            if active.contains(&key) {
                continue;
            }
            let pending = Self::orphan_queue_pending_at(roots, &key);
            // The probe's SH guard and SQLite connection have now ended.
            // Retire only without another active reader/leader on this index.
            let lock = parent.join(format!("{key}.lock"));
            if let Ok(_exclusive) = topology::UseGuard::acquire_existing_readonly_exclusive(&lock) {
                let dir = parent.join(&key);
                drop(SqliteWitnessRetirement(vec![
                    dir.join("index.db"),
                    dir.join("requests.db"),
                ]));
            }
            if pending.unwrap_or(true) {
                return true;
            }
        }
        false
    }

    fn orphan_queue_pending_at(roots: &topology::TopologyRoots, key: &str) -> Result<bool> {
        use sha2::{Digest, Sha256};
        ensure!(
            key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid index key"
        );
        let dir = roots.cache.join("indexes").join(key);
        let metadata = std::fs::symlink_metadata(&dir)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "unsafe index directory"
        );
        let guard = topology::UseGuard::acquire_existing_readonly(
            &roots.cache.join("indexes").join(format!("{key}.lock")),
        )?;
        let path = dir.join("index.db");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let absent = requests::queue_absent_without_sidecars(&dir.join("requests.db"));
                guard.verify()?;
                return Ok(!absent);
            }
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
        // SQLite may recover a hot journal or create shared-memory files even
        // through a read-only connection. Check all sidecars before opening it.
        reject_sidecars(&path, true)?;
        let db = open_index_marker_probe(&path, false, Duration::ZERO)?;
        let (spelling, device, inode): (String, String, String) = db.query_row(
            "SELECT root_spelling,root_device,root_inode FROM index_metadata WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        drop(db);
        ensure!(
            hex::encode(Sha256::digest(spelling.as_bytes())) == key,
            "index identity mismatch"
        );
        let root = Path::new(&spelling);
        // Only a proved pathname loss or new root inode is disposable. Unreadable
        // paths, malformed metadata and marker errors are not proof that the old
        // queue may be abandoned.
        use std::os::unix::fs::MetadataExt;
        let old_device = device.parse::<u64>()?;
        let old_inode = inode.parse::<u64>()?;
        match std::fs::symlink_metadata(root) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
            Ok(named)
                if !named.is_dir()
                    || named.file_type().is_symlink()
                    || (named.dev(), named.ino()) != (old_device, old_inode) =>
            {
                return Ok(false);
            }
            Ok(_) => {}
        }
        let identity = topology::WorkspaceIdentity::discover_unattached(Some(root), root)?
            .attach_existing_marker_readonly()?;
        ensure!(
            identity.root_key == key
                && identity.device == old_device
                && identity.inode == old_inode,
            "index root changed"
        );
        guard.verify()?;
        let store = Self::unopened(roots.clone(), identity)?;
        let pending = store.open_maintenance_queue_probe()?.check() != MaintenanceQueueState::Clear;
        guard.verify()?;
        Ok(pending)
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
        roots.prepare_index(&identity)?;
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
                    *store.obsolete_format_marker.lock().unwrap() = Some(obsolete.0.clone());
                }
                store.mark_recovery(RecoveryDisposition::RecreatePending);
                Ok(store)
            }
            Err(error) if recovery_class(&error) == RecoveryClass::Rebuild => {
                if let Some(obsolete) = error.downcast_ref::<ObsoleteIndexFormat>() {
                    *store.obsolete_format_marker.lock().unwrap() = Some(obsolete.0.clone());
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
        let raw_options = raw_options.context("incompatible_index: missing reconcile options")?;
        let options: crate::indexer::ReconcileOptions = serde_json::from_str(&raw_options)
            .context("incompatible_index: invalid reconcile options")?;
        ensure!(
            json(&options)? == raw_options && options.version == 1,
            "incompatible_index: unsupported reconcile options"
        );
        options.require_absolute_optional_inputs().context(
            "recovery_required: recorded relative index input; run explicit trellis index",
        )?;
        let status = store.read_status(&db)?;
        store.identity.verify_readonly()?;
        guard.verify()?;
        Ok(status)
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
                db.execute_batch(CACHE_SCHEMA_V8)?;
                db.execute_batch(SUPERSESSION_SCHEMA_V8)?;
                db.execute_batch(RETENTION_SCHEMA_V8)?;
                db.execute_batch(RETENTION_FK_INDEX_SCHEMA_V8)?;
                db.execute_batch(REVISION_HEADER_INDEX_SCHEMA_V8)?;
                db.execute_batch(REVISION_DOCUMENT_PATH_INDEX_SCHEMA_V8)?;
                let age = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                ensure!(age <= 9_007_199_254_740_991, "invalid_open_age");
                db.execute(
                    "INSERT INTO index_metadata VALUES(1,11,?1,?2,?3,?4,?5,0,?6,'',?7,?8,NULL)",
                    params![
                        EXTRACTOR_VERSION,
                        self.workspace_root,
                        self.identity.device.to_string(),
                        self.identity.inode.to_string(),
                        uuid::Uuid::new_v4().to_string(),
                        age as i64,
                        json(&IndexStats::default())?,
                        json(&Vec::<Diagnostic>::new())?
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
                        initial_marker.as_ref() == Some(&found.0),
                        "recovery_required: obsolete marker changed since admission"
                    );
                    let db = open_index_marker_probe(&path, false, Duration::ZERO)?;
                    self.verify_metadata_root(&db)?;
                    ensure!(
                        read_index_format_marker(&db)?.eq(&found.0),
                        "recovery_required: obsolete marker changed under EX"
                    );
                    obsolete = Some(found.0.clone());
                }
                recovery_class(&error)
            }
            Ok(db) => {
                let result = self.recovery_baseline_full(&db);
                drop(db);
                match result {
                    Err(error) => recovery_class(&error),
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
            self.verify_metadata_root(&db)?;
            ensure!(
                read_index_format_marker(&db)?.eq(marker),
                "recovery_required: obsolete marker changed before stage"
            );
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
                    self.verify_metadata_root(&db)?;
                    ensure!(
                        read_index_format_marker(&db)?.eq(marker),
                        "recovery_required: obsolete marker changed before rename"
                    );
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
        let use_guard = match self.roots.index_use(&self.identity) {
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
        drop(exclusive);
        Ok(())
    }
    fn cache_write(&self) -> Result<IndexConnection> {
        self.connect_index(true)
    }
    fn connect_index(&self, writable: bool) -> Result<IndexConnection> {
        self.ensure_not_recreate_pending()?;
        self.identity.verify()?;
        let use_guard = self.roots.index_use_existing(&self.identity)?;
        let path = self.roots.index_db(&self.identity);
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
    }
    fn records(&self) -> topology::DurableRecords<'_> {
        topology::DurableRecords::new(&self.roots, &self.identity)
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
        self.recovery_required.store(true, Ordering::Release);
    }
    #[cfg(feature = "test-support")]
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
    fn classify_admission_error(&self, error: anyhow::Error, typed: bool) -> Result<bool> {
        let error = if typed {
            selected_integrity(error)
        } else {
            error
        };
        match recovery_class(&error) {
            RecoveryClass::Rebuild => {
                self.mark_recovery(RecoveryDisposition::Rebuild);
                Ok(false)
            }
            RecoveryClass::RecreatePending => {
                self.mark_recovery(RecoveryDisposition::RecreatePending);
                Ok(false)
            }
            RecoveryClass::Hard => Err(error),
        }
    }
    fn report_live_read_failure(&self, error: anyhow::Error) -> anyhow::Error {
        match recovery_class(&error) {
            RecoveryClass::Rebuild => {
                self.mark_recovery(RecoveryDisposition::Rebuild);
                anyhow::anyhow!("incompatible_index: live index decode failed: {error:#}")
            }
            RecoveryClass::RecreatePending => {
                self.mark_recovery(RecoveryDisposition::RecreatePending);
                anyhow::anyhow!("recovery_required: exceptional index recovery deferred")
            }
            RecoveryClass::Hard => error,
        }
    }
    fn report_selected_failure(&self, error: anyhow::Error) -> anyhow::Error {
        match recovery_class(&error) {
            RecoveryClass::Rebuild => {
                self.mark_recovery(RecoveryDisposition::Rebuild);
                anyhow::anyhow!("incompatible_index: selected evidence decode failed: {error:#}")
            }
            RecoveryClass::RecreatePending => {
                self.mark_recovery(RecoveryDisposition::RecreatePending);
                anyhow::anyhow!("incompatible_index: exceptional index recovery deferred")
            }
            RecoveryClass::Hard => error,
        }
    }
    fn verify_metadata_root(&self, db: &Connection) -> Result<()> {
        let (count, typed): (i64, bool) = db.query_row(
            "SELECT count(*),coalesce(min(singleton=1 AND typeof(root_spelling)='text' AND typeof(root_device)='text' AND typeof(root_inode)='text'),0) FROM index_metadata",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        ensure!(
            count == 1 && typed,
            "root_key_collision: index root identity is not a typed singleton"
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
        self.verify_metadata_root(db)?;
        let witness = self.metadata_witness(db, schema)?;
        if schema > DATABASE_SCHEMA_VERSION {
            anyhow::bail!("incompatible_index: unsupported newer schema cannot be rebaselined");
        }
        if schema != DATABASE_SCHEMA_VERSION {
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
                let compatible = self.classify_admission_error(error.into(), false)?;
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
                    false,
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
                Err(error) => self.classify_admission_error(error, true)?,
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
        )?;
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
            stats: serde_json::from_str(&row.8)?,
            diagnostics: serde_json::from_str(&row.9)?,
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
            .map_err(|error| self.report_live_read_failure(error))?;
        self.ensure_public_read_ready()?;
        self.decode_control_status_raw(db)
            .map_err(|error| self.report_live_read_failure(error))
    }

    fn read_status(&self, db: &Connection) -> Result<IndexStatus> {
        self.read_status_for(db, false)
    }
    fn read_status_for(&self, db: &Connection, pre_startup: bool) -> Result<IndexStatus> {
        let status = if pre_startup {
            self.verify_metadata_root(db)
                .map_err(|error| self.report_live_read_failure(error))?;
            // startup reconciliation may keep a valid prior head readable, but a selected document
            // already proved corrupt cannot be admitted by that same path.
            if self.disposition() == RecoveryDisposition::Rebuild {
                anyhow::bail!(
                    "incompatible_index: reconciliation required after invalid current index"
                );
            }
            self.decode_control_status_raw(db)
                .map_err(|error| self.report_live_read_failure(error))?
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
        .map_err(|error| self.report_live_read_failure(error))?;
        Ok(status)
    }

    /// Internal control baseline, never returned by public status or evidence reads.
    pub fn index_baseline(&self) -> Result<IndexPin> {
        self.recovery_index_baseline()?
            .pin
            .context("index_not_ready: prior index pin is not decodable")
    }
    pub(crate) fn recovery_index_baseline(&self) -> Result<RecoveryBaseline> {
        self.ensure_not_recreate_pending()?;
        let mut db = self.cache()?;
        let tx = storage_result(db.transaction())?;
        begin_read_snapshot(
            &tx,
            &self.roots.index_db(&self.identity),
            Duration::from_secs(5),
        )?;
        self.recovery_baseline(&tx)
    }
    // Only publication callers may defer retained/header checks: the selected
    // head is verified at open and the resulting head is checked before commit.
    pub(crate) fn publication_index_baseline(&self) -> Result<RecoveryBaseline> {
        self.ensure_not_recreate_pending()?;
        let mut db = self.cache()?;
        let tx = storage_result(db.transaction())?;
        begin_read_snapshot(
            &tx,
            &self.roots.index_db(&self.identity),
            Duration::from_secs(5),
        )?;
        self.recovery_baseline_preflight(&tx)
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
    pub fn workspace_root(&self) -> &str {
        &self.workspace_root
    }
    pub fn recorded_index_options(&self) -> Result<Option<crate::indexer::IndexOptions>> {
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
        self.verify_metadata_root(&tx)?;
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
        let payload = payload.context("incompatible_index: missing reconcile options")?;
        let options: crate::indexer::ReconcileOptions = serde_json::from_str(&payload)
            .context("incompatible_index: invalid reconcile options")?;
        ensure!(
            json(&options)? == payload && options.version == 1,
            "incompatible_index: unsupported reconcile options"
        );
        options.require_absolute_optional_inputs().context(
            "recovery_required: recorded relative index input; run explicit trellis index",
        )?;
        let mut result =
            crate::indexer::IndexOptions::new(Path::new(&self.workspace_root).to_owned());
        result.max_file_bytes = options.max_file_bytes;
        result.scip_path = options.scip_path.map(Into::into);
        result.manifest_path = options.manifest_path.map(Into::into);
        Ok(Some(result))
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
        extractions: &BTreeMap<String, crate::classes::FileExtraction>,
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
    /// Managed private scratch directory for an unpublished full-document spool.
    /// The coordinator must use this Store's roots, including isolated test roots.
    pub(crate) fn native_spool_dir(&self) -> Result<std::path::PathBuf> {
        self.identity.verify()?;
        self.roots.prepare_index(&self.identity)?;
        Ok(self.roots.index_dir(&self.identity))
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
        self.ensure_not_recreate_pending()?;
        self.identity.verify()?;
        let db = self
            .cache()
            .map_err(|error| self.report_live_read_failure(error))?;
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
        Ok(EvidenceResponse {
            store,
            db,
            fence: ReadFence(selected.pin),
        })
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
        let value = read(&response);
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
            .map_err(|error| self.report_live_read_failure(error))?;
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
        )
    }

    fn publish_validated_stream_target(
        &self,
        spool: &crate::native_evidence::FullDocumentSpool,
        capture: &crate::capture::Capture,
        leader: &topology::WriterGuard,
        expected: RecoveryBaseline,
        target: PublicationTarget<'_>,
        cancel: &CancelFlag,
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
        capture
            .reconcile_options()
            .require_absolute_optional_inputs()?;
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
            |_, _| Ok(()),
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
        for file in &capture.files {
            if file.path == prepared.changed_path {
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
        self.publish_inner_checked_target_with_local(
            (graph, capture, &native),
            leader,
            PublicationPlan {
                expected: ExpectedPublication::Recovery(Box::new(expected)),
                target: PublicationTarget::Live,
            },
            cancel,
            (256 * 1024 * 1024 + 16 * 1024, Some(&prepared), None),
            |_, _| Ok(()),
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
        // A capture from a direct library caller must not persist cwd-relative
        // presentation identities even if it bypassed the explicit CLI ingress.
        capture
            .reconcile_options()
            .require_absolute_optional_inputs()
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
        let scope = v8_native_scope_for(db, &key, selected)?
            .context("incompatible_index: reusable native version absent")?;
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
                WHERE d.id=?1 AND d.source_set_id=?2 AND d.language=?3 AND d.path=?4",
            params![scope.version_id, key.source_set_id, key.language, key.path],
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
        ensure!(
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
        let observed = crate::native_evidence::document_witness(
            &document,
            &producer,
            &coverage,
            &declarations,
            &calls,
            &control_regions,
        )?;
        ensure!(
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
        })
    }

    /// Option C: admit a proven single-document body edit without decoding any
    /// unchanged occurrence or projection row. IDs must come from the selected
    /// same-generation head, never an orphan or a matching historical version.
    /// A leader Serve can advance an unchanged, already-validated head without
    /// re-extracting native facts. This is deliberately stricter than stat
    /// equality: Capture::admit has freshly read and SHA-256 hashed every source,
    /// and selected_source_row_for authenticates the saved bytes of every reused
    /// version before any metadata-only writer transaction starts.
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
        if !has_revision_producer_bindings(&db)? {
            return Ok(false);
        }
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
        let comparison = compare_capture_snapshot(&db, capture)?;
        if comparison.examined == 0 || comparison.changed() {
            return Ok(false);
        }
        for source in &capture.files {
            check_cancel(cancel)?;
            let stored = self
                .selected_source_row_for(&db, &source.path, &selected)?
                .context("incompatible_index: selected final inventory source missing")?;
            ensure!(
                stored == *source,
                "incompatible_index: selected final inventory source differs"
            );
            let linked: Option<(String, String, String, String, String)> = db.query_row(
                "SELECT g.state,c.state,v.extraction_context,v.producer_id,v.producer_version
                 FROM revision_documents m
                 JOIN document_versions v ON v.id=m.document_version_id AND v.language=m.language AND v.path=m.path
                 JOIN graph_projections g ON g.id=m.graph_projection_id AND g.document_version_id=m.document_version_id AND g.language=m.language
                 JOIN class_projections c ON c.id=m.class_projection_id AND c.graph_projection_id=m.graph_projection_id
                 WHERE m.revision_id=?1 AND m.path=?2",
                 params![selected.key, source.path],
                 |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
            ).optional()?;
            let (graph, class, context, producer, version) = linked
                .context("incompatible_index: selected final inventory projection missing")?;
            ensure!(
                graph == "ready"
                    && class == "ready"
                    && producer == crate::native_evidence::PRODUCER
                    && version == crate::native_evidence::NATIVE_VERSION
                    && context
                        == crate::native_evidence::declared_selected_extraction_context(
                            &producer,
                            &version,
                            &source.language
                        )?,
                "incompatible_index: selected final inventory producer or projection drift"
            );
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
        ensure!(
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
        ensure!(
            class_warning_bytes.is_some_and(|bytes| (0..=256 * 1024).contains(&bytes)),
            "incompatible_index: selected class catalog budget exceeded or missing"
        );
        if !has_revision_producer_bindings(&db)? {
            // Legacy 28-table indexes use the ordinary FULL publication path,
            // which atomically introduces the paired producer-binding extension.
            return Ok(None);
        }
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
        let comparison = compare_capture_snapshot_for_reuse(&db, capture)?;
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
        for source in &capture.files {
            check_cancel(cancel)?;
            let stored = self
                .selected_source_row_for(&db, &source.path, &selected)?
                .context("incompatible_index: selected reused source missing")?;
            ensure!(
                stored == *source,
                "incompatible_index: selected reused source bytes differ"
            );
            // The prior head has already validated these immutable versions.
            // Check their indexed version/projection links without re-parsing
            // millions of native and graph facts. Selected reads still deeply
            // attest a requested projection before returning its evidence.
            let linked: Option<(String, String, String, String, String)> = db
                .query_row(
                    "SELECT g.state,c.state,v.extraction_context,v.producer_id,v.producer_version
                 FROM revision_documents m
                 JOIN document_versions v ON v.id=m.document_version_id
                   AND v.language=m.language AND v.path=m.path
                 JOIN graph_projections g ON g.id=m.graph_projection_id
                   AND g.document_version_id=m.document_version_id AND g.language=m.language
                 JOIN class_projections c ON c.id=m.class_projection_id
                   AND c.graph_projection_id=m.graph_projection_id
                 WHERE m.revision_id=?1 AND m.path=?2",
                    params![selected.key, source.path],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .optional()?;
            let (graph_state, class_state, context, producer_id, producer_version) =
                linked.context("incompatible_index: selected reusable projection link missing")?;
            ensure!(
                graph_state == "ready"
                    && class_state == "ready"
                    && producer_id == crate::native_evidence::PRODUCER
                    && producer_version == crate::native_evidence::NATIVE_VERSION
                    && context
                        == crate::native_evidence::declared_selected_extraction_context(
                            &producer_id,
                            &producer_version,
                            &source.language,
                        )?,
                "incompatible_index: selected reusable projection link or extraction context invalid"
            );
        }
        // The same connection fences the authenticated preflight against a
        // concurrent direct SQL rewrite. The leader/use guards exclude normal
        // writers, while the metadata CAS excludes stale in-process requests.
        let after_preflight: i64 =
            storage_result(db.pragma_query_value(None, "data_version", |row| row.get(0)))?;
        ensure!(
            after_preflight == admitted_version,
            "incompatible_index: index changed during unchanged preflight"
        );
        capture.verify(cancel)?;
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
            return Ok(Some(selected.pin));
        };
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
        let copied = tx.execute(
                "INSERT INTO native_revisions SELECT ?1,source_set_id,?4,config_hash,             dependency_hash,?5,?6,dependency_observations,             reconcile_options,class_warnings,class_truncated,graph_stats,graph_diagnostics,?2             FROM native_revisions WHERE id=?3",
                params![
                    revision_key,
                    revision.index_revision as i64,
                    selected.key,
                    header.toolchain_hash,
                    header.id,
                    json(&header.documents)?
                ],
            )?;
        ensure!(
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
            "INSERT INTO revision_documents SELECT ?1,source_set_id,language,path,document_version_id,             graph_projection_id,class_projection_id,capture_stat,coverage_requested,             coverage_selected,coverage_state,coverage_diagnostic,ordinal             FROM revision_documents WHERE revision_id=?2",
            params![revision_key, selected.key],
        )?;
        ensure!(
            copied == capture.files.len(),
            "incompatible_index: incomplete unchanged revision manifest"
        );
        let (bound_hash, binding_sha) = revision_producer_binding(&tx, &revision_key)?;
        ensure!(
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
        stamp_predecessor(&tx, &selected.key, publication_time.as_secs() as i64)?;
        ensure!(
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
        use crate::{indexer::CapturedChange, native_evidence};
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
            // Decision 0005 (2026-10 amendment): a proved single-body edit may
            // reuse unchanged documents under a rebuilt executable only while
            // the extractor producer hash is unchanged. measure_persisted_change
            // compares that recorded hash; a changed one takes the full path.
            // The selected head is a prior validated publication. Do not decode
            // its unchanged fact/projection rows; selected reads still attest
            // them, and the current captured source bytes are checked below.
            let previous_options: crate::indexer::ReconcileOptions = db.query_row(
                "SELECT reconcile_options FROM index_metadata WHERE singleton=1", [],
                |r| r.get::<_, String>(0))?.parse::<serde_json::Value>()
                    .and_then(serde_json::from_value)?;
            let mut previous_inputs = BTreeMap::new();
            for row in db.prepare("SELECT input_key,payload FROM revision_capture_inputs WHERE revision_id=?1 ORDER BY input_key")?
                .query_map([&selected.key], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            {
                let (key, payload) = row?;
                ensure!(previous_inputs.insert(key, serde_json::from_str(&payload)?).is_none(),
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
            let mut prior_extractions = BTreeMap::new();
            let mut changed_counts = BTreeMap::new();
            let source_set_id = format!("source-set:v1:{}", self.root_id());
            let producer_id = crate::native_evidence::PRODUCER;
            let producer_version = crate::native_evidence::NATIVE_VERSION;
            // Unchanged prior documents are trusted by their stored, validated
            // content hash (owner ruling, #141): their bytes are not read,
            // re-hashed or decoded here. Only the changed document's bytes are
            // loaded and witness-checked below. Selected reads still attest
            // what they serve.
            let mut statement = db.prepare("SELECT m.source_set_id,m.path,m.language,v.content_hash,v.byte_length,NULL,
                v.extraction_context,v.producer_id,v.producer_version,m.document_version_id,
                m.graph_projection_id,m.class_projection_id,g.graph_hash,c.content_hash,
                g.class_extraction_payload,m.coverage_requested,m.coverage_selected,
                m.coverage_state,m.coverage_diagnostic
                FROM revision_documents m JOIN document_versions v ON v.id=m.document_version_id
                JOIN graph_projections g ON g.id=m.graph_projection_id
                LEFT JOIN class_projections c ON c.id=m.class_projection_id
                WHERE m.revision_id=?1 ORDER BY m.ordinal")?;
            for row in statement.query_map([&selected.key], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?, r.get::<_, i64>(4)?, (),
                    r.get::<_, String>(6)?, r.get::<_, String>(7)?, r.get::<_, String>(8)?,
                    r.get::<_, String>(9)?, r.get::<_, String>(10)?,
                    r.get::<_, Option<String>>(11)?, r.get::<_, String>(12)?,
                    r.get::<_, Option<String>>(13)?, r.get::<_, Option<String>>(14)?,
                    r.get::<_, bool>(15)?, r.get::<_, bool>(16)?,
                    r.get::<_, String>(17)?, r.get::<_, Option<String>>(18)?))
            })? {
                let (row_source_set_id,path,language,hash,length,(),context,id,version,version_id,
                    graph_id,class_id,graph_hash,class_hash,extraction,requested,covered,state,diagnostic)=row?;
                ensure!(row_source_set_id == source_set_id,
                    "incompatible_index: prior manifest source-set identity mismatch");
                ensure!(length >= 0 && hash.len() == 64,
                    "incompatible_index: prior captured source witness mismatch");
                ensure!(id == producer_id && version == producer_version
                    && context == crate::native_evidence::declared_selected_extraction_context(
                        &id, &version, &language,
                    )?
                    && class_id.is_some() && class_hash.is_some(),
                    "incompatible_index: prior document producer/context missing");
                let source = SourceFile {path:path.clone(),language:language.clone(),hash:hash.clone(),text:String::new()};
                previous.push(source);
                ensure!(prior_ids.insert(path.clone(), V8DocumentProjection {
                    version_id, graph_id, class_id:class_id.context("class projection missing")?,
                    graph_hash,class_hash:class_hash.context("class hash missing")?,
                }).is_none(),"incompatible_index: duplicate local manifest path");
                prior_coverage.insert(path.clone(), native_evidence::Coverage {
                    producer_id:producer_id.to_owned(),language:language.clone(),
                    source_set_id:source_set_id.clone(),document_path:path.clone(),
                    revision_id:String::new(),requested,selected:covered,state,diagnostic,
                    supported_roles:vec!["definition".into(),"call".into()],
                    observed_roles:vec!["definition".into(),"call".into()],
                });
                if let Some(payload) = extraction {
                    prior_extractions.insert(path.clone(), serde_json::from_str(&payload)?);
                }
                changed_counts.insert(path, (0_usize,0_usize,0_usize));
            }
            // The one document whose old text matters (proved_body_only reads
            // it) is the only one whose stored bytes are read and checked.
            let current_hashes: BTreeMap<&str, &str> =
                capture.files.iter().map(|f| (f.path.as_str(), f.hash.as_str())).collect();
            let changed_paths: Vec<usize> = previous.iter().enumerate()
                .filter(|(_, old)| current_hashes.get(old.path.as_str()).is_some_and(|new| *new != old.hash))
                .map(|(i, _)| i).collect();
            if let [i] = changed_paths[..] {
                let old = &mut previous[i];
                let (length, bytes): (i64, Vec<u8>) = db.query_row(
                    "SELECT v.byte_length,v.source_bytes FROM revision_documents m
                     JOIN document_versions v ON v.id=m.document_version_id
                     WHERE m.revision_id=?1 AND m.path=?2 AND m.language=?3",
                    params![selected.key, old.path, old.language], |r| Ok((r.get(0)?, r.get(1)?)))?;
                ensure!(length >= 0 && length as usize == bytes.len()
                    && old.hash == hex::encode(sha2::Sha256::digest(&bytes)),
                    "incompatible_index: prior captured source witness mismatch");
                old.text = String::from_utf8(bytes)?;
            }
            let decision = crate::indexer::measure_persisted_change(
                &previous, &previous_options, &previous_inputs, capture)?;
            let path = match decision {
                CapturedChange::DocumentLocal {path} => path,
                _ => return Ok(None),
            };
            let old_coverage=prior_coverage.get(&path).context("local coverage missing")?;
            if old_coverage.state != "complete" { return Ok(None); }
            let changed_file=capture.files.iter().find(|f|f.path==path).context("changed file missing")?;
            let stored=prior_ids.get(&path).context("prior changed version missing")?;
            // The captured root has one source-set identity; the selected SQL
            // rows above must belong to it. Reject collisions instead of letting
            // a same-path row in another language silently shadow an old fact.
            let previous_by_key: BTreeMap<(&str, &str, &str), &SourceFile> = previous
                .iter()
                .map(|f| ((source_set_id.as_str(), f.language.as_str(), f.path.as_str()), f))
                .collect();
            ensure!(previous_by_key.len() == previous.len(),
                "incompatible_index: duplicate prior document identity");
            let changed_key = (source_set_id.as_str(), changed_file.language.as_str(), path.as_str());
            let prior_changed = previous_by_key.get(&changed_key)
                .context("incompatible_index: changed document identity missing")?;
            ensure!(prior_changed.language == changed_file.language
                && prior_changed.hash != changed_file.hash,
                "local edit did not change captured content hash");
            // The new document is extracted and fully checked; prior versions
            // are linked only by exact captured identity and selected ancestry.
            for file in &capture.files {
                if file.path != path {
                    let key = (source_set_id.as_str(), file.language.as_str(), file.path.as_str());
                    let old = previous_by_key.get(&key)
                        .context("incompatible_index: unchanged document identity missing")?;
                    ensure!(old.language == file.language && old.hash == file.hash,
                        "incompatible_index: unchanged document identity mismatch");
                }
            }
            let measured=native_evidence::measure_captured_document(
                capture,Path::new(&self.workspace_root),self.root_id(),&path,cancel,|_|{})?;
            if measured.coverage.state != "complete" { return Ok(None); }
            let native=native_evidence::validated_changed_artifact(capture,measured,cancel)?;
            // The prior graph counts are used only for this changed path's
            // header delta, not to authorize any old semantic fact.
            let count = |table: &str| -> Result<usize> {
                Ok(db.query_row(&format!("SELECT count(*) FROM {table} WHERE projection_id=?1"),
                    [&stored.graph_id],|r|r.get::<_,i64>(0))? as usize)
            };
            let old_changed_counts=(count("graph_nodes")?,count("graph_calls")?,count("graph_regions")?);
            let (stats,diagnostics,warnings,truncated):(String,String,String,bool)=db.query_row(
                "SELECT m.stats,m.diagnostics,r.class_warnings,r.class_truncated
                 FROM index_metadata m JOIN native_revisions r ON r.id=?1 WHERE m.singleton=1",
                [&selected.key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?;
            let _: BTreeMap<_, _> = changed_counts;
            Ok(Some(LocalPrepared {changed_path:path,native,prior_pin:selected.pin,
                prior_ids,prior_coverage,prior_extractions,
                old_stats:serde_json::from_str(&stats)?,old_diagnostics:serde_json::from_str(&diagnostics)?,
                old_class_warnings:serde_json::from_str(&warnings)?,old_class_truncated:truncated,
                old_changed_counts}))
        })
    }

    /// Finalize a changed-only graph before claiming the capture's sole graph
    /// projection. A moving class cap or changed per-file F uses the existing
    /// full-native same-capture fallback instead of reusing an unproved class.
    pub(crate) fn compose_local_revision(
        &self,
        capture: &crate::capture::Capture,
        prepared: &LocalPrepared,
        changed: Graph,
        cancel: &CancelFlag,
    ) -> Result<Option<Graph>> {
        ensure!(
            changed.files.len() == 1 && changed.files[0].path == prepared.changed_path,
            "selected graph must cover exactly the measured document"
        );
        crate::indexer::validate_native_graph_records(&changed, &prepared.native, cancel)?;
        let mut stats = prepared.old_stats.clone();
        let (old_nodes, old_calls, old_regions) = prepared.old_changed_counts;
        stats.symbols = stats
            .symbols
            .checked_sub(old_nodes)
            .and_then(|n| n.checked_add(changed.nodes.len()))
            .context("incompatible_index: invalid prior symbol summary")?;
        stats.calls = stats
            .calls
            .checked_sub(old_calls)
            .and_then(|n| n.checked_add(changed.calls.len()))
            .context("incompatible_index: invalid prior call summary")?;
        stats.regions = stats
            .regions
            .checked_sub(old_regions)
            .and_then(|n| n.checked_add(changed.regions.len()))
            .context("incompatible_index: invalid prior region summary")?;
        stats.unresolved = stats.calls;
        ensure!(
            stats.files == capture.files.len()
                && !prepared
                    .old_diagnostics
                    .iter()
                    .any(|d| d.path.as_deref() == Some(&prepared.changed_path))
                && changed.diagnostics.is_empty(),
            "incompatible_index: local graph summary differs from complete coverage"
        );
        let composed = Graph {
            files: capture.files.clone(),
            nodes: changed.nodes,
            calls: changed.calls,
            regions: changed.regions,
            diagnostics: prepared.old_diagnostics.clone(),
            stats,
            ..Graph::default()
        };
        if matches!(
            composed
                .files
                .iter()
                .find(|f| f.path == prepared.changed_path)
                .map(|f| f.language.as_str()),
            Some("java" | "python")
        ) {
            let file = composed
                .files
                .iter()
                .find(|f| f.path == prepared.changed_path)
                .context("changed class source absent")?;
            let extracted = crate::classes::FileExtraction::extract_file(
                file,
                &composed.nodes,
                cancel,
                crate::classes::Limits::default(),
            )?;
            if prepared.prior_extractions.get(&file.path) != Some(&extracted) {
                let mut next = prepared.prior_extractions.clone();
                ensure!(
                    next.insert(file.path.clone(), extracted).is_some(),
                    "incompatible_index: prior class extraction absent"
                );
                let catalog = Self::compose_selected_class_catalog(
                    &composed,
                    &next,
                    crate::classes::Limits::default(),
                )?;
                // If this method-body edit moves a class cap or warning, use
                // the full same-capture fallback. Equal F takes the original
                // one-composition fast path and never pays a second O(files).
                if catalog.warnings != prepared.old_class_warnings
                    || catalog.truncated != prepared.old_class_truncated
                {
                    return Ok(None);
                }
            }
        }
        Ok(Some(composed))
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
        use crate::{indexer::CapturedChange, native_evidence};
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
                    .and_then(serde_json::from_value)?;
            let mut previous_inputs=BTreeMap::new();
            for row in db.prepare("SELECT input_key,payload FROM revision_capture_inputs WHERE revision_id=?1 ORDER BY input_key")?
                .query_map([&selected.key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))? {
                let (key,payload)=row?;
                ensure!(previous_inputs.insert(key,serde_json::from_str(&payload)?).is_none(),
                    "incompatible_index: duplicate prior capture input");
            }
            let mut previous=vec![];
            for row in db.prepare("SELECT d.path,d.language,d.content_hash,d.byte_length,d.source_bytes
                FROM revision_documents m JOIN document_versions d ON d.id=m.document_version_id
                WHERE m.revision_id=?1 ORDER BY m.ordinal")?
                .query_map([&selected.key],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,
                    r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,Vec<u8>>(4)?)))? {
                use sha2::{Digest,Sha256};
                let (path,language,hash,length,bytes)=row?;
                ensure!(length>=0 && length as usize==bytes.len()
                    && bytes.len()<=256*1024*1024
                    && hex::encode(Sha256::digest(&bytes))==hash,
                    "incompatible_index: prior captured source witness mismatch");
                previous.push(SourceFile {path,language,hash,text:String::from_utf8(bytes)?});
            }
            let decision=crate::indexer::measure_persisted_change(
                &previous,&previous_options,&previous_inputs,capture)?;
            let selected_path=match decision {
                CapturedChange::FullNative {..} => return Ok(None),
                CapturedChange::DocumentLocal {path} => Some(path),
                CapturedChange::Unchanged => None,
            };
            let root=Path::new(&self.workspace_root);
            let mut measured=vec![];
            for file in &capture.files {
                if selected_path.as_deref()==Some(&file.path) {
                    // A previously failed/partial source cannot be transferred as a
                    // successful local proof, even if the changed token is body-only.
                    let previous_file=previous.iter().find(|old|old.path==file.path)
                        .context("prior local source absent")?;
                    let old=self.selected_reusable_native(db,&selected,previous_file).context("incompatible_index: prior native witness invalid")?;
                    if old.coverage.state!="complete" { return Ok(None); }
                    let changed=native_evidence::measure_captured_document(
                        capture,root,self.root_id(),&file.path,cancel, |_| {})?;
                    if changed.coverage.state!="complete" { return Ok(None); }
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
    /// validated them, so they are not decoded again here; selected reads
    /// still attest what they serve. A mismatch is typed index corruption.
    fn verify_reusable_projection(
        &self,
        db: &Connection,
        ids: &V8DocumentProjection,
        reuse: ReusedFamilies,
        extraction: Option<&crate::classes::FileExtraction>,
    ) -> Result<()> {
        if reuse.graph {
            let (version, hash, state, class_state, stored_f):
                (String,String,String,String,Option<String>) = db.query_row(
                "SELECT document_version_id,graph_hash,state,class_extraction_state,class_extraction_payload
                    FROM graph_projections WHERE id=?1",
                [&ids.graph_id],
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
                "SELECT graph_projection_id,content_hash FROM class_projections WHERE id=?1",
                [&ids.class_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
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
            ensure!(
                files.len() == prepared.prior_ids.len(),
                "incompatible_index: local manifest inventory changed"
            );
            let mut families = BTreeMap::new();
            let mut existing_ids = BTreeMap::new();
            let mut count = 0;
            let mut statement = db.prepare(
                "SELECT m.path,m.language,m.document_version_id,
                m.graph_projection_id,m.class_projection_id,v.content_hash,v.extraction_context,
                v.producer_id,v.producer_version
                FROM revision_documents m JOIN document_versions v ON v.id=m.document_version_id
                WHERE m.revision_id=?1 ORDER BY m.ordinal",
            )?;
            for row in statement.query_map([&selected.key], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
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
                ensure!(
                    old.version_id == version_id
                        && old.graph_id == graph_id
                        && Some(old.class_id.as_str()) == class_id.as_deref(),
                    "incompatible_index: local selected manifest changed"
                );
                let file = files.get(path.as_str()).context("local source missing")?;
                ensure!(
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
                if path != prepared.changed_path {
                    ensure!(
                        hash == file.hash,
                        "incompatible_index: reusable captured content hash differs"
                    );
                    ensure!(
                        families
                            .insert(
                                path.clone(),
                                ReusedFamilies {
                                    native: true,
                                    graph: true,
                                    class: true,
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
                            class_id: old.class_id.clone(),
                            graph_hash: old.graph_hash.clone(),
                            class_hash: old.class_hash.clone(),
                        },
                    );
                }
            }
            ensure!(
                count == files.len() && existing_ids.len() + 1 == count,
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
        extractions: &BTreeMap<String, crate::classes::FileExtraction>,
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
                "SELECT path,document_version_id,graph_projection_id,class_projection_id
                FROM revision_documents WHERE revision_id=?1 ORDER BY path",
            )?;
            for row in stmt.query_map([&selected.key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })? {
                let (path, version, projection, class) = row?;
                ensure!(
                    old.insert(path, (version, projection, class)).is_none(),
                    "incompatible_index: duplicate prior manifest document"
                );
            }
            // R-1: compare each fresh projection ID and native witness with
            // the stored hashes Trellis wrote and validated at publication,
            // instead of re-reading and re-canonicalizing every stored row.
            // Both are pure per-document values, computed on workers.
            let never = AtomicBool::new(false);
            let computed = crate::parallel::map_ordered(
                &graph.files,
                crate::parallel::default_workers(),
                &never,
                |_, file| -> Result<(V8DocumentProjection, Option<String>)> {
                    let grouped =
                        rows.get(file.path.as_str()).context("publication rows missing")?;
                    let ids = v8_document_projection(file, native, grouped)?;
                    let reused_version =
                        old.get(&file.path).is_some_and(|(version, _, _)| *version == ids.version_id);
                    let witness = if drift || reused_version {
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
                    Ok((ids, witness))
                },
            );
            let mut families = BTreeMap::new();
            for (file, computed) in graph.files.iter().zip(computed) {
                let (ids, measured) = computed.context("preflight projection absent")??;
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
                    ensure!(
                        !reused.graph || reused.native,
                        "incompatible_index: graph reused with different native version"
                    );
                    ensure!(
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

    fn prepare_stream_class_catalog(
        spool: &crate::native_evidence::FullDocumentSpool,
        order: &[usize],
        cancel: &CancelFlag,
    ) -> Result<crate::classes::Catalog> {
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
        for index in replay {
            if !composer.is_open() {
                break;
            }
            let document = &spool.header().revision.documents[index];
            if matches!(document.key.language.as_str(), "java" | "python") {
                check_cancel(cancel)?;
                let extraction = spool
                    .read_extraction(index)
                    .map_err(native_spool_read_failure)?;
                if !composer.push(&extraction) {
                    break;
                }
            }
        }
        Ok(composer.finish())
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
                "SELECT path,document_version_id,graph_projection_id,class_projection_id
                FROM revision_documents WHERE revision_id=?1 ORDER BY path",
            )?;
            for row in stmt.query_map([&selected.key], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })? {
                let (path, version, projection, class) = row?;
                ensure!(
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
                    ensure!(
                        !reused.graph || reused.native,
                        "incompatible_index: graph reused with different native version"
                    );
                    ensure!(
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
        )
    }

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
    ) -> Result<IndexPin> {
        let PublicationPlan { expected, target } = plan;
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
        let (stats, classes, extractions) = if let Some(spool) = stream {
            ensure!(
                local.is_none(),
                "local and streamed publication cannot be combined"
            );
            (
                spool.stats().clone(),
                Self::prepare_stream_class_catalog(spool, &stream_order, cancel)?,
                BTreeMap::new(),
            )
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
            let extractions: BTreeMap<String, crate::classes::FileExtraction> =
                if let Some(prepared) = local {
                    ensure!(
                        prepared.prior_extractions.len() == class_paths.len(),
                        "incompatible_index: local class extraction inventory incomplete"
                    );
                    let mut extractions = prepared.prior_extractions.clone();
                    if let Some(file) = graph.files.iter().find(|f| {
                        f.path == prepared.changed_path
                            && matches!(f.language.as_str(), "java" | "python")
                    }) {
                        let extracted = crate::classes::FileExtraction::extract_file(
                            file,
                            class_symbols
                                .get(file.path.as_str())
                                .map(Vec::as_slice)
                                .unwrap_or(&[]),
                            cancel,
                            limits,
                        )?;
                        ensure!(
                            extractions.insert(file.path.clone(), extracted).is_some(),
                            "incompatible_index: prior class extraction absent"
                        );
                    }
                    extractions
                } else {
                    graph
                        .files
                        .iter()
                        .filter(|f| matches!(f.language.as_str(), "java" | "python"))
                        .map(|file| {
                            Ok((
                                file.path.clone(),
                                crate::classes::FileExtraction::extract_file(
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
                        .collect::<Result<_>>()?
                };
            let classes = Self::compose_selected_class_catalog(graph, &extractions, limits)?;
            if let Some(prepared) = local {
                ensure!(
                    classes.warnings == prepared.old_class_warnings
                        && classes.truncated == prepared.old_class_truncated,
                    "incompatible_index: local class catalog differs from trusted head"
                );
            }
            (stats, classes, extractions)
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
        let mut db = match target {
            PublicationTarget::Live => PublicationConnection::Live(
                self.cache_write()
                    .map_err(|error| self.report_live_read_failure(error))?,
            ),
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
            self.preflight_local_reuse(graph, prepared)?
        } else if let Some(spool) = stream {
            self.preflight_stream_reuse(spool, &stream_order, &classes, &expected, cancel)
                .map_err(|error| self.report_live_read_failure(error))?
        } else if matches!(target, PublicationTarget::Live) {
            // A stored hash that differs from the fresh one is typed index
            // corruption: the next publication rebaselines from fresh facts.
            self.preflight_reuse(graph, native, &extractions, &expected, &rows)
                .map_err(|error| self.report_live_read_failure(error))?
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
        let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let current = match target {
            PublicationTarget::Live => self
                .recovery_baseline_preflight(&tx)
                .map_err(|error| self.report_live_read_failure(error))?,
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
        if rebaseline {
            if !has_revision_supersessions(&tx)? {
                tx.execute_batch(SUPERSESSION_SCHEMA_V8)?;
            }
            if !has_revision_release_debt(&tx)? {
                tx.execute_batch(RETENTION_SCHEMA_V8)?;
            }
            if !has_retention_fk_indexes(&tx)? {
                tx.execute_batch(RETENTION_FK_INDEX_SCHEMA_V8)?;
            }
            if !has_revision_header_cover(&tx)? {
                tx.execute_batch(REVISION_HEADER_INDEX_SCHEMA_V8)?;
            }
            if !has_revision_document_path_index(&tx)? {
                tx.execute_batch(REVISION_DOCUMENT_PATH_INDEX_SCHEMA_V8)?;
            }
        } else {
            install_supersessions(&tx, self.retention_time()?.as_secs() as i64)?;
        }
        let binding_extension = has_revision_producer_bindings(&tx)?;
        if rebaseline && binding_extension {
            tx.execute_batch(
                "DELETE FROM revision_producer_bindings; DELETE FROM native_binding_epoch;",
            )?;
        }
        if rebaseline {
            // An incompatible/corrupt generation is explicitly replaced, never served.
            // Only compatible same-generation publication appends history.
            tx.execute_batch("DELETE FROM revision_documents; DELETE FROM revision_capture_inputs;
                DELETE FROM native_version_call_regions; DELETE FROM native_version_calls;
                DELETE FROM native_version_control_regions; DELETE FROM native_version_own_signature_types;
                DELETE FROM native_version_ancestor_signature_types; DELETE FROM native_version_parameters;
                DELETE FROM native_version_header_items; DELETE FROM native_version_headers;
                DELETE FROM native_version_declaration_ancestors; DELETE FROM native_version_declarations;
                DELETE FROM native_version_coverage_roles; DELETE FROM class_relations; DELETE FROM classes;
                DELETE FROM class_projections; DELETE FROM graph_calls; DELETE FROM graph_regions;
                DELETE FROM graph_nodes; DELETE FROM graph_projections; DELETE FROM document_versions;
                DELETE FROM native_release_candidate_classes; DELETE FROM native_release_candidate_graphs;
                DELETE FROM native_release_candidate_versions; DELETE FROM native_revision_release_debt;
                DELETE FROM native_revision_supersessions; DELETE FROM native_revisions; DELETE FROM native_source_set_dependencies;
                DELETE FROM native_source_set_languages; DELETE FROM native_source_sets;
                DELETE FROM native_producer_inputs; DELETE FROM native_producer_languages;
                DELETE FROM native_producers;")?;
        }
        if !binding_extension {
            // DDL and epoch are committed with the FIRST newly bound revision.
            // Readers before commit see the original exact v8 shape and old pins.
            tx.execute_batch(PRODUCER_BINDING_SCHEMA_V8)?;
        }
        if !binding_extension || rebaseline {
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
        let classify_immutable = |error| match target {
            PublicationTarget::Live => self.report_live_read_failure(error),
            PublicationTarget::Stage(_) => error,
        };
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
                NativeWrite::Header { ordinal_start: 0 },
            )
            .map_err(classify_immutable)?;
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
                            && artifact.revision.toolchain_hash == native.revision.toolchain_hash
                            && artifact.revision.config_hash == native.revision.config_hash
                            && artifact.revision.dependency_hash == native.revision.dependency_hash
                            && artifact.revision.documents.as_slice()
                                == std::slice::from_ref(&native.revision.documents[index]),
                        "native_evidence_required: streamed revision header differs"
                    );
                    let file = document_graph
                        .files
                        .first()
                        .context("stream graph document absent")?;
                    ensure!(
                        document_graph.files.len() == 1 && source_files.get(ordinal) == Some(file),
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
                    )
                    .map_err(classify_immutable)?;
                    let ids = written
                        .get(&file.path)
                        .context("missing streamed document projection")?;
                    during_tx(PublishStage::AfterFile, &tx)?;
                    let grouped = rows
                        .get(file.path.as_str())
                        .context("streamed publication rows missing")?;
                    write_graph_document(
                        &tx,
                        grouped,
                        ids,
                        reuse.for_path(&file.path),
                        &mut immutable,
                    )
                    .map_err(classify_immutable)?;
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
                NativeWrite::Header { ordinal_start: 0 },
            )
            .map_err(classify_immutable)?
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
                let ids = projections
                    .get(&f.path)
                    .context("missing graph projection")?;
                let grouped = rows
                    .get(f.path.as_str())
                    .context("publication rows missing")?;
                write_graph_document(&tx, grouped, ids, reuse.for_path(&f.path), &mut immutable)
                    .map_err(classify_immutable)?;
            }
        }
        immutable.finish(&tx).map_err(classify_immutable)?;
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
        tx.execute("UPDATE index_metadata SET schema_version=11,extractor_version=?1,index_generation=?2,index_revision=?3,indexed_at=?4,stats=?5,diagnostics=?6,reconcile_options=?7 WHERE singleton=1",
            params![EXTRACTOR_VERSION, revision.index_generation.to_string(), revision.index_revision as i64,timestamp,json(&stats)?,json(&graph.diagnostics)?,json(capture.reconcile_options())?])?;
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
        let head = self.read_status_for(db, pre_startup)?.revision;
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
        selected
            .producer_hash(db)
            .map_err(selected_integrity)
            .map_err(|error| self.report_selected_failure(error))?;
        Ok(selected)
    }

    fn native_at<T>(
        &self,
        pin: IndexPin,
        read: impl FnOnce(&Connection, &ReadRevision) -> Result<T>,
    ) -> Result<T> {
        self.with_evidence(|db| {
            let selected = self.read_revision(db, Some(pin))?;
            read(db, &selected).map_err(|error| self.report_selected_failure(error))
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
            if !has_revision_release_debt(&db)? {
                guard.verify()?;
                return Ok(None);
            }
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

    /// Deterministic rollback after a complete index rebuild but before COMMIT.
    #[doc(hidden)]
    pub fn set_publication_before_commit_hook_for_tests(
        &self,
        hook: impl FnOnce() + Send + 'static,
    ) {
        self.publication_before_commit_hook.set(hook);
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
        if !has_revision_release_debt(&db)? {
            guard.verify()?;
            return Ok(MaintenanceOutcome::Idle);
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
                    WHERE revision_id=?1 AND state='retained' AND superseded_at<=?2",
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
                ensure!(
                    state == "pending",
                    "incompatible_index: invalid pending debt state"
                );
                let entries: Vec<(String, String, Option<String>, i64)> = tx
                    .prepare(
                        "SELECT document_version_id,graph_projection_id,class_projection_id,ordinal
                     FROM revision_documents WHERE revision_id=?1 ORDER BY ordinal LIMIT 64",
                    )?
                    .query_map([&selected], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
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
                if !has_manifest && !has_candidates {
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
        ensure!(
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
        let unreferenced_class = "SELECT p.id FROM class_projections p WHERE NOT EXISTS
            (SELECT 1 FROM revision_documents m WHERE m.class_projection_id=p.id)";
        for table in ["class_relations", "classes"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE projection_id IN ({unreferenced_class})"),
                [],
            )?;
        }
        tx.execute(
            &format!("DELETE FROM class_projections WHERE id IN ({unreferenced_class})"),
            [],
        )?;
        let unreferenced_graph = "SELECT p.id FROM graph_projections p WHERE NOT EXISTS
            (SELECT 1 FROM revision_documents m WHERE m.graph_projection_id=p.id)";
        for table in ["graph_calls", "graph_regions", "graph_nodes"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE projection_id IN ({unreferenced_graph})"),
                [],
            )?;
        }
        tx.execute(
            &format!("DELETE FROM graph_projections WHERE id IN ({unreferenced_graph})"),
            [],
        )?;
        let unreferenced_version = "SELECT v.id FROM document_versions v WHERE NOT EXISTS
            (SELECT 1 FROM revision_documents m WHERE m.document_version_id=v.id)";
        for table in [
            "native_version_call_regions",
            "native_version_calls",
            "native_version_control_regions",
            "native_version_header_items",
            "native_version_parameters",
            "native_version_headers",
            "native_version_ancestor_signature_types",
            "native_version_own_signature_types",
            "native_version_declaration_ancestors",
            "native_version_declarations",
            "native_version_coverage_roles",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE version_id IN ({unreferenced_version})"),
                [],
            )?;
        }
        tx.execute(
            &format!("DELETE FROM document_versions WHERE id IN ({unreferenced_version})"),
            [],
        )?;
        Self::check_retained_foreign_keys(&tx)?;
        leader.verify()?;
        self.identity.verify()?;
        tx.commit()?;
        Ok(())
    }

    /// The producer, source set and revision header one selected, paired
    /// source is reparsed under, read in this SQLite snapshot. `file` is the
    /// selected source the caller read and hash-checked in this snapshot.
    fn selected_witness_inputs(
        &self,
        db: &Connection,
        path: &str,
        file: &SourceFile,
        selected: &ReadRevision,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<WitnessInputs> {
        optional_hint_check(progress)?;
        use crate::native_evidence::{Document, DocumentKey};
        let (source_set_id, revision_id): (String, String) = db
            .query_row(
                "SELECT m.source_set_id,r.native_revision_id FROM revision_documents m
                 JOIN native_revisions r ON r.id=m.revision_id
                 WHERE m.path=?1 AND m.revision_id=?2",
                params![path, selected.key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .context("incompatible_index: selected native document absent")?;
        ensure!(
            source_set_id == format!("source-set:v1:{}", self.root_id()),
            "incompatible_index: selected source set mismatch"
        );
        ensure!(
            file.path == path,
            "incompatible_index: selected graph source absent"
        );
        // Every document of one selected revision shares its producer, source
        // set and revision header; read them once per snapshot.
        let fresh;
        let headers = match selected.headers.witness.get() {
            Some(headers)
                if headers.source_set.id == source_set_id && headers.revision.id == revision_id =>
            {
                headers
            }
            Some(_) => {
                fresh = Self::selected_witness_headers(db, selected, &source_set_id, &revision_id)?;
                &fresh
            }
            None => {
                let headers =
                    Self::selected_witness_headers(db, selected, &source_set_id, &revision_id)?;
                selected.headers.witness.get_or_init(|| headers)
            }
        };
        let mut revision = headers.revision.clone();
        revision.documents.push(Document {
            key: DocumentKey {
                source_set_id,
                language: file.language.clone(),
                path: path.into(),
            },
            revision_id,
            content_hash: file.hash.clone(),
            byte_length: file.text.len(),
        });
        Ok(WitnessInputs {
            producer: headers.producer.clone(),
            source_set: headers.source_set.clone(),
            revision,
        })
    }

    fn selected_witness_headers(
        db: &Connection,
        selected: &ReadRevision,
        source_set_id: &str,
        revision_id: &str,
    ) -> Result<WitnessHeaders> {
        use crate::native_evidence::{Producer, Revision, SourceSet};
        let mut producer:Producer=db.query_row(
            "SELECT id,version,executable_hash,kind,position_encoding FROM native_producers LIMIT 1",[],
            |r|Ok(Producer{id:r.get(0)?,version:r.get(1)?,executable_hash:r.get(2)?,kind:r.get(3)?,languages:vec![],position_encoding:r.get(4)?}),
        )?;
        producer.executable_hash = selected.producer_hash(db)?;
        producer.languages=db.prepare(
            "SELECT language FROM native_producer_languages WHERE producer_id=?1 AND producer_version=?2 ORDER BY ordinal"
        )?.query_map(params![producer.id,producer.version],|r|r.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut source_set: SourceSet = db.query_row(
            "SELECT id,root_id FROM native_source_sets WHERE id=?1",
            [source_set_id],
            |r| {
                Ok(SourceSet {
                    id: r.get(0)?,
                    root_id: r.get(1)?,
                    languages: vec![],
                    dependencies: vec![],
                })
            },
        )?;
        source_set.languages=db.prepare(
            "SELECT language FROM native_source_set_languages WHERE source_set_id=?1 ORDER BY ordinal"
        )?.query_map([source_set_id],|r|r.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        source_set.dependencies=db.prepare(
            "SELECT dependency_id FROM native_source_set_dependencies WHERE source_set_id=?1 ORDER BY ordinal"
        )?.query_map([source_set_id],|r|r.get::<_,String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let revision:Revision=db.query_row(
            "SELECT native_revision_id,source_set_id,toolchain_hash,config_hash,dependency_hash FROM native_revisions WHERE native_revision_id=?1 AND id=?2",
            params![revision_id, selected.key],|r|Ok(Revision{id:r.get(0)?,source_set_id:r.get(1)?,
                documents:vec![],toolchain_hash:r.get(2)?,config_hash:r.get(3)?,dependency_hash:r.get(4)?}),
        )?;
        Ok(WitnessHeaders {
            producer,
            source_set,
            revision,
        })
    }

    /// Authenticate one selected document's normalized records and graph DTOs
    /// against its paired BLOB in the same pinned SQLite transaction.
    // SQL aggregate touches only the selected path's ancillary native and class rows.
    // SourceFile JSON and graph node/call/region payloads have separate envelopes.
    fn selected_ancillary_byte_usage(
        db: &Connection,
        version_id: &str,
        class_id: &str,
    ) -> Result<(i64, i64)> {
        Ok(db.query_row(
            SELECTED_ANCILLARY_BYTE_SQL,
            params![version_id, class_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
    fn attest_selected_document_for(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
    ) -> Result<()> {
        self.attest_selected_document_inner(db, path, selected, None)
            .map_err(selected_integrity)
            .map_err(|error| self.report_selected_failure(error))
    }
    fn attest_selected_document_optional_for(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
        expired: &dyn Fn() -> bool,
    ) -> Result<()> {
        self.attest_selected_document_loaded_for(db, path, selected, expired)
            .map(drop)
    }
    /// Attest one selected document and return the source and native rows it
    /// authenticated, so a caller can use them without reading them again.
    fn attest_selected_document_loaded_for(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
        expired: &dyn Fn() -> bool,
    ) -> Result<AttestedDocument> {
        self.attest_selected_document_loaded(db, path, selected, Some(expired))
            .map_err(selected_integrity)
            .map_err(|error| self.report_selected_failure(error))
    }
    fn attest_selected_document_inner(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<()> {
        self.attest_selected_document_loaded(db, path, selected, progress)
            .map(drop)
    }
    fn attest_selected_document_loaded(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<AttestedDocument> {
        let header = self.fetch_selected_header(db, path, selected, progress)?;
        let fetched = self.fetch_selected_rows(db, header, selected, progress)?;
        Self::attest_fetched(fetched, progress)
    }
    /// Read, in this snapshot and in order, everything attestation compares
    /// for one selected document: first its manifest row and the SQL row and
    /// byte budgets (`fetch_selected_header`), then, only after those budgets
    /// pass, the rows they bound (`fetch_selected_rows`). Stored rows are
    /// decoded and structurally checked as they are read. Nothing is compared
    /// with the source here: `attest_fetched` does that, off the connection.
    fn fetch_selected_header(
        &self,
        db: &Connection,
        path: &str,
        selected: &ReadRevision,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<SelectedHeader> {
        #[cfg(test)]
        count_selected_read(|counts| counts.attestations += 1);
        use crate::native_evidence::DocumentKey;
        optional_hint_check(progress)?;
        let sql = "SELECT m.source_set_id,m.language,m.path,m.document_version_id,m.graph_projection_id,
            m.class_projection_id,d.content_hash,d.byte_length,length(d.source_bytes),
            g.graph_hash,g.state,g.class_extraction_state,length(CAST(g.class_extraction_payload AS BLOB)),
            c.content_hash,c.state,r.native_revision_id
            FROM revision_documents m JOIN native_revisions r ON r.id=m.revision_id
            JOIN document_versions d ON d.id=m.document_version_id
              AND d.source_set_id=m.source_set_id AND d.language=m.language AND d.path=m.path
            JOIN graph_projections g ON g.id=m.graph_projection_id
              AND g.document_version_id=d.id AND g.language=d.language
            LEFT JOIN class_projections c ON c.id=m.class_projection_id AND c.graph_projection_id=g.id
            WHERE m.path=?1 AND m.revision_id=?2 LIMIT 2";
        let mut stmt = db.prepare(sql)?;
        optional_hint_check(progress)?;
        let rows = stmt
            .query_map(params![path, selected.key], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
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
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
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
        ) = rows.into_iter().next().unwrap();
        ensure!(
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
        let class_id = class_id.context("incompatible_index: selected class projection missing")?;
        ensure!(
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
        ensure!(
            matches!(
                (language.as_str(), class_state.as_str()),
                ("java" | "python", "ready") | ("javascript" | "rust", "notApplicable")
            ),
            "incompatible_index: class extraction state invalid"
        );
        let key = DocumentKey {
            source_set_id: source_set,
            language: language.clone(),
            path: path.into(),
        };
        let scope = v8_native_scope_for(db, &key, selected)?
            .context("incompatible_index: selected native version missing")?;
        ensure!(
            scope.version_id == version_id && scope.native_revision_id == native_revision_id,
            "incompatible_index: selected native manifest mismatch"
        );
        let row_limit = bytes.saturating_mul(16).clamp(1024, 1_000_000);
        let counts_sql = "SELECT (SELECT count(*) FROM graph_nodes WHERE projection_id=?1),
            (SELECT count(*) FROM graph_calls WHERE projection_id=?1),
            (SELECT count(*) FROM graph_regions WHERE projection_id=?1),
            (SELECT count(*) FROM classes WHERE projection_id=?2),
            (SELECT count(*) FROM class_relations WHERE projection_id=?2),
            (SELECT count(*) FROM native_version_coverage_roles WHERE version_id=?3),
            (SELECT count(*) FROM native_version_declarations WHERE version_id=?3),
            (SELECT count(*) FROM native_version_declaration_ancestors WHERE version_id=?3),
            (SELECT count(*) FROM native_version_own_signature_types WHERE version_id=?3),
            (SELECT count(*) FROM native_version_ancestor_signature_types WHERE version_id=?3),
            (SELECT count(*) FROM native_version_headers WHERE version_id=?3),
            (SELECT count(*) FROM native_version_header_items WHERE version_id=?3),
            (SELECT count(*) FROM native_version_parameters WHERE version_id=?3),
            (SELECT count(*) FROM native_version_calls WHERE version_id=?3),
            (SELECT count(*) FROM native_version_control_regions WHERE version_id=?3),
            (SELECT count(*) FROM native_version_call_regions WHERE version_id=?3)";
        let counts: Vec<i64> =
            db.query_row(counts_sql, params![graph_id, class_id, version_id], |r| {
                (0..16)
                    .map(|i| r.get(i))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?;
        ensure!(
            counts.iter().all(|&n| n >= 0 && n <= row_limit),
            "incompatible_index: selected evidence row budget exceeded"
        );
        optional_hint_check(progress)?;
        let (max_row, total_bytes) =
            Self::selected_ancillary_byte_usage(db, &version_id, &class_id)?;
        ensure!(
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
        let graph_byte_sql="SELECT COALESCE(max(row_bytes),0),COALESCE(sum(row_bytes),0) FROM (
            SELECT length(CAST(id AS BLOB))+length(CAST(name AS BLOB))+length(CAST(path AS BLOB))+length(CAST(payload AS BLOB)) AS row_bytes FROM graph_nodes WHERE projection_id=?1
            UNION ALL SELECT length(CAST(id AS BLOB))+length(CAST(caller AS BLOB))+COALESCE(length(CAST(target AS BLOB)),0)+length(CAST(path AS BLOB))+length(CAST(payload AS BLOB)) FROM graph_calls WHERE projection_id=?1
            UNION ALL SELECT length(CAST(id AS BLOB))+length(CAST(owner AS BLOB))+length(CAST(path AS BLOB))+length(CAST(payload AS BLOB)) FROM graph_regions WHERE projection_id=?1)";
        optional_hint_check(progress)?;
        let (graph_row, graph_total): (i64, i64) =
            db.query_row(graph_byte_sql, [&graph_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        ensure!(
            graph_row >= 0
                && graph_total >= 0
                && graph_row
                    <= bytes
                        .saturating_mul(6)
                        .saturating_add(16 * 1024)
                        .min(256 * 1024 * 1024 + 16 * 1024)
                && graph_total
                    <= bytes
                        .saturating_mul(64)
                        .saturating_add(256 * 1024)
                        .min(512 * 1024 * 1024),
            "incompatible_index: selected graph row byte budget exceeded"
        );
        // Everything the rows below will hold, as the budgets above measured it.
        let stored_bytes = [bytes, total_bytes, graph_total, f_length.unwrap_or(0)]
            .iter()
            .map(|&n| u64::try_from(n).unwrap_or(0))
            .sum();
        Ok(SelectedHeader {
            path: path.to_owned(),
            language,
            version_id,
            graph_id,
            class_id,
            graph_hash,
            class_hash,
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
        let SelectedHeader {
            path,
            language,
            version_id,
            graph_id,
            class_id,
            graph_hash,
            class_hash,
            hash,
            bytes,
            scope,
            stored_bytes: _,
        } = header;
        let path = path.as_str();
        let file = Self::selected_source_row_bounded_for_inner(
            db,
            path,
            256 * 1024 * 1024,
            selected,
            progress,
        )?
        .context("incompatible_index: selected source missing")?;
        ensure!(
            file.hash == hash && file.language == language && file.text.len() == bytes as usize,
            "incompatible_index: selected source identity mismatch"
        );
        let witness = self.selected_witness_inputs(db, path, &file, selected, progress)?;
        let coverage = read_native_coverage_v8(db, &scope)?;
        let declarations = read_native_declarations_v8_inner(db, &scope, None, true, progress)?;
        optional_hint_check(progress)?;
        let calls = read_native_calls_v8_inner(db, &scope, None, progress)?;
        optional_hint_check(progress)?;
        let regions = read_native_control_regions_v8_inner(db, &scope, None, progress)?;
        let mut node_rows = Vec::new();
        for row in db
            .prepare_cached(
                "SELECT id,name,path,payload FROM graph_nodes WHERE projection_id=?1 ORDER BY id",
            )?
            .query_map([&graph_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            })?
        {
            optional_hint_check(progress)?;
            node_rows.push(row?);
        }
        let mut call_rows = Vec::new();
        for row in db.prepare_cached("SELECT id,caller,target,path,payload FROM graph_calls WHERE projection_id=?1 ORDER BY id")?
            .query_map([&graph_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))? {
            optional_hint_check(progress)?;
            call_rows.push(row?);
        }
        let mut region_rows = Vec::new();
        for row in db.prepare_cached("SELECT id,owner,path,payload FROM graph_regions WHERE projection_id=?1 ORDER BY id")?
            .query_map([&graph_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))? {
            optional_hint_check(progress)?;
            region_rows.push(row?);
        }
        let mut class_rows = Vec::new();
        for row in db.prepare_cached("SELECT id,name,qualified_name,path,payload FROM classes WHERE projection_id=?1 ORDER BY id")?
            .query_map([&class_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?)))? {
            optional_hint_check(progress)?;
            class_rows.push(row?);
        }
        let mut relation_rows = Vec::new();
        for row in db.prepare_cached("SELECT id,owner,target,payload FROM class_relations WHERE projection_id=?1 ORDER BY id")?
            .query_map([&class_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,String>(3)?)))? {
            optional_hint_check(progress)?;
            relation_rows.push(row?);
        }
        optional_hint_check(progress)?;
        let stored_f: Option<String> = db
            .prepare_cached("SELECT class_extraction_payload FROM graph_projections WHERE id=?1")?
            .query_row([&graph_id], |r| r.get(0))?;
        Ok(FetchedDocument {
            language,
            version_id,
            graph_id,
            class_id,
            graph_hash,
            class_hash,
            file,
            witness,
            coverage,
            declarations,
            calls,
            regions,
            node_rows,
            call_rows,
            region_rows,
            class_rows,
            relation_rows,
            stored_f,
        })
    }

    /// Authenticate one fetched document against its source: reparse it,
    /// then compare the stored native rows, graph and class DTOs, F and the
    /// projection fingerprint with what the source yields. This touches no
    /// SQLite handle, so it may run on a worker thread.
    fn attest_fetched(
        fetched: FetchedDocument,
        progress: Option<&dyn Fn() -> bool>,
    ) -> Result<AttestedDocument> {
        let FetchedDocument {
            language,
            version_id,
            graph_id,
            class_id,
            graph_hash,
            class_hash,
            file,
            witness,
            coverage,
            declarations: native_declarations,
            calls: native_calls,
            regions: native_regions,
            node_rows,
            call_rows,
            region_rows,
            class_rows: stored_class_rows,
            relation_rows,
            stored_f,
        } = fetched;
        let witness = crate::native_evidence::selected_source_witness_with_progress(
            &file,
            witness.producer,
            witness.source_set,
            witness.revision,
            progress,
        )
        .context("incompatible_index: selected source extraction failed")?;
        ensure!(
            witness.coverage == vec![coverage],
            "incompatible_index: selected native coverage differs from source"
        );
        // The store's query order (by syntax_id) is what callers receive; the
        // comparison sorts both sides without reordering the returned rows.
        let mut declarations: Vec<&crate::native_evidence::Declaration> =
            native_declarations.iter().collect();
        optional_hint_check(progress)?;
        let mut expected = Vec::new();
        for declaration in &witness.declarations {
            optional_hint_check(progress)?;
            expected.push(declaration);
        }
        declarations.sort_by(|a, b| a.syntax_id.cmp(&b.syntax_id));
        expected.sort_by(|a, b| a.syntax_id.cmp(&b.syntax_id));
        ensure!(
            declarations == expected,
            "incompatible_index: selected native declarations differ from source"
        );
        optional_hint_check(progress)?;
        // Each version's calls and regions were decoded once, ordered (owner,
        // ordinal) by the store's byte-order query; compare whole lists.
        let mut expected_calls = witness.calls.clone();
        expected_calls
            .sort_by(|a, b| (&a.owner_syntax_id, a.ordinal).cmp(&(&b.owner_syntax_id, b.ordinal)));
        ensure!(
            native_calls == expected_calls,
            "incompatible_index: selected native calls differ from source"
        );
        optional_hint_check(progress)?;
        let mut expected_regions = witness.control_regions.clone();
        expected_regions
            .sort_by(|a, b| (&a.owner_syntax_id, a.ordinal).cmp(&(&b.owner_syntax_id, b.ordinal)));
        ensure!(
            native_regions == expected_regions,
            "incompatible_index: selected native regions differ from source"
        );
        let mut nodes = Vec::new();
        for (id, name, path, payload) in node_rows {
            optional_hint_check(progress)?;
            let node: Symbol = serde_json::from_str(&payload)?;
            ensure!(
                node.id == id && node.name == name && node.path == path && path == file.path,
                "incompatible_index: selected graph node materialization differs"
            );
            nodes.push(node);
        }
        let mut calls = Vec::new();
        for (id, caller, target, path, payload) in call_rows {
            optional_hint_check(progress)?;
            let call: CallSite = serde_json::from_str(&payload)?;
            ensure!(
                call.id == id
                    && call.caller == caller
                    && call.path == path
                    && path == file.path
                    && target.is_none(),
                "incompatible_index: selected graph call materialization differs"
            );
            calls.push(call);
        }
        let mut regions = Vec::new();
        for (id, owner, path, payload) in region_rows {
            optional_hint_check(progress)?;
            let region: ControlRegion = serde_json::from_str(&payload)?;
            ensure!(
                region.id == id
                    && region.owner == owner
                    && region.path == path
                    && path == file.path,
                "incompatible_index: selected graph region materialization differs"
            );
            regions.push(region);
        }
        let mut class_rows = Vec::new();
        for (id, name, qualified, path, payload) in stored_class_rows {
            optional_hint_check(progress)?;
            let class: crate::classes::ClassDefinition = serde_json::from_str(&payload)?;
            ensure!(
                class.symbol.id == id
                    && class.symbol.name == name
                    && class.symbol.path == path
                    && class.qualified_name == qualified
                    && path == file.path,
                "incompatible_index: selected class materialization differs"
            );
            class_rows.push(class);
        }
        let mut class_relations = Vec::new();
        for (id, owner, target, payload) in relation_rows {
            optional_hint_check(progress)?;
            let relation: crate::classes::ClassRelation = serde_json::from_str(&payload)?;
            ensure!(
                relation.id == id && relation.owner == owner && relation.target == target,
                "incompatible_index: selected class relation materialization differs"
            );
            class_relations.push(relation);
        }
        optional_hint_check(progress)?;
        let mut graph = Graph {
            files: vec![file],
            nodes,
            calls,
            regions,
            ..Graph::default()
        };
        graph.stats.files = 1;
        graph.stats.symbols = graph.nodes.len();
        graph.stats.calls = graph.calls.len();
        graph.stats.regions = graph.regions.len();
        graph.stats.unresolved = graph.calls.len();
        for coverage in &witness.coverage {
            optional_hint_check(progress)?;
            if coverage.state != "complete" {
                let recovered = coverage
                    .diagnostic
                    .as_deref()
                    .is_some_and(|d| d.contains("parser recovered"));
                if recovered {
                    graph.stats.parse_error_files += 1;
                }
                graph.diagnostics.push(Diagnostic {
                    path: Some(coverage.document_path.clone()),
                    code: if recovered {
                        "parse-error"
                    } else {
                        "native-coverage-partial"
                    }
                    .into(),
                    message: coverage
                        .diagnostic
                        .clone()
                        .unwrap_or_else(|| "Native extraction is incomplete".into()),
                });
            }
        }
        optional_hint_check(progress)?;
        crate::indexer::validate_native_graph_records_with_progress(
            &graph,
            &witness,
            &Arc::new(std::sync::atomic::AtomicBool::new(false)),
            progress,
        )?;
        if matches!(language.as_str(), "java" | "python") {
            optional_hint_check(progress)?;
            let expected_f = crate::classes::FileExtraction::extract_file_with_progress(
                &graph.files[0],
                &graph.nodes,
                &Arc::new(std::sync::atomic::AtomicBool::new(false)),
                crate::classes::Limits::default(),
                progress,
            )?;
            let recorded_f: Option<crate::classes::FileExtraction> = stored_f
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok());
            optional_hint_check(progress)?;
            ensure!(
                recorded_f.as_ref() == Some(&expected_f),
                "incompatible_index: selected F differs from authenticated source and graph"
            );
        } else {
            ensure!(
                stored_f.is_none(),
                "incompatible_index: non-class graph carries F"
            );
        }
        optional_hint_check(progress)?;
        let catalog = crate::classes::Catalog {
            classes: class_rows,
            relations: class_relations,
            warnings: vec![],
            truncated: false,
        };
        optional_hint_check(progress)?;
        let rows = publication_rows_with_progress(&graph, &witness, &catalog, progress)?;
        let grouped = rows
            .get(graph.files[0].path.as_str())
            .context("selected rows missing")?;
        optional_hint_check(progress)?;
        let ids =
            v8_document_projection_with_progress(&graph.files[0], &witness, grouped, progress)?;
        ensure!(
            ids.version_id == version_id
                && ids.graph_id == graph_id
                && ids.class_id == class_id
                && ids.graph_hash == graph_hash
                && Some(ids.class_hash) == class_hash,
            "incompatible_index: selected projection fingerprint mismatch"
        );
        Ok(AttestedDocument {
            file: graph.files.swap_remove(0),
            declarations: native_declarations,
            calls: native_calls,
            regions: native_regions,
        })
    }

    /// Class DTOs are selected presentation projections; compare only this
    /// document's rows with a bounded in-memory class build from attested bytes.
    /// Attest only the returned paths under one pinned read transaction. The
    /// other files' previously validated F rows are immutable inputs to the
    /// class catalog (T00); never re-parse the whole selected workspace here.
    fn attest_selected_classes_for(
        &self,
        db: &Connection,
        paths: &BTreeSet<&str>,
        selected: &ReadRevision,
    ) -> Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        for path in paths {
            self.attest_selected_document_for(db, path, selected)?;
        }
        self.attest_selected_classes_inner(db, paths, selected)
            .map_err(selected_integrity)
            .map_err(|error| self.report_selected_failure(error))
    }

    fn attest_selected_classes_inner(
        &self,
        db: &Connection,
        paths: &BTreeSet<&str>,
        selected: &ReadRevision,
    ) -> Result<()> {
        let limits = crate::classes::Limits::default();
        let (stats_length, raw_stats, raw_warnings, truncated): (i64, String, String, bool) = db
            .query_row(
            "SELECT length(CAST(graph_stats AS BLOB)),graph_stats,class_warnings,class_truncated
             FROM native_revisions WHERE id=?1",
            [&selected.key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?;
        ensure!(
            (0..=GRAPH_FIELD_MAX_BYTES).contains(&stats_length) && raw_warnings.len() <= 256 * 1024,
            "incompatible_index: selected class metadata byte budget exceeded"
        );
        let stats: IndexStats = serde_json::from_str(&raw_stats)?;
        let warnings: Vec<String> = serde_json::from_str(&raw_warnings)?;
        let (file_count, class_file_count): (i64, i64) = db.query_row(
            "SELECT count(*),sum(CASE WHEN language IN ('java','python') THEN 1 ELSE 0 END)
             FROM revision_documents WHERE revision_id=?1",
            [&selected.key],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        ensure!(
            file_count >= 0 && file_count as usize == stats.files && class_file_count >= 0,
            "incompatible_index: selected class inventory differs from graph summary"
        );
        let mut extracts = Vec::new();
        let mut f_bytes = 0usize;
        let mut statement = db.prepare(
            "SELECT m.path,g.class_extraction_payload FROM revision_documents m
             JOIN graph_projections g ON g.id=m.graph_projection_id
               AND g.document_version_id=m.document_version_id
             WHERE m.revision_id=?1 AND m.language IN ('java','python') ORDER BY m.path",
        )?;
        for row in statement.query_map([&selected.key], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })? {
            let (path, raw_f) = row?;
            f_bytes = f_bytes
                .checked_add(raw_f.len())
                .context("incompatible_index: selected F byte budget overflow")?;
            ensure!(
                raw_f.len() <= 64 * 1024 * 1024 && f_bytes <= 512 * 1024 * 1024,
                "incompatible_index: selected F byte budget exceeded"
            );
            let extract: crate::classes::FileExtraction = serde_json::from_str(&raw_f)?;
            ensure!(
                extract.path == path,
                "incompatible_index: selected F document identity differs"
            );
            extracts.push(extract);
        }
        ensure!(
            extracts.len() == class_file_count as usize,
            "incompatible_index: selected class extraction inventory incomplete"
        );
        let catalog =
            crate::classes::Catalog::compose(&extracts, stats.files, stats.symbols, limits)?;
        ensure!(
            catalog.warnings == warnings && catalog.truncated == truncated,
            "incompatible_index: selected class catalog metadata differs"
        );
        for path in paths {
            let (language, class_id): (String, String) = db.query_row(
                "SELECT language,class_projection_id FROM revision_documents
                 WHERE revision_id=?1 AND path=?2",
                params![selected.key, path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            if !matches!(language.as_str(), "java" | "python") {
                continue;
            }
            let mut expected: Vec<_> = catalog
                .classes
                .iter()
                .filter(|class| class.symbol.path == *path)
                .cloned()
                .collect();
            let mut actual: Vec<crate::classes::ClassDefinition> = db
                .prepare("SELECT payload FROM classes WHERE projection_id=?1 ORDER BY id")?
                .query_map([&class_id], |r| r.get::<_, String>(0))?
                .map(|payload| Ok(serde_json::from_str(&payload?)?))
                .collect::<Result<_>>()?;
            expected.sort_by(|a, b| a.symbol.id.cmp(&b.symbol.id));
            actual.sort_by(|a, b| a.symbol.id.cmp(&b.symbol.id));
            ensure!(
                actual == expected,
                "incompatible_index: selected class projection differs from stored F catalog"
            );
            let mut expected_relations: Vec<_> = catalog
                .relations
                .iter()
                .filter(|relation| relation.path == *path)
                .cloned()
                .collect();
            let mut actual_relations: Vec<crate::classes::ClassRelation> = db
                .prepare("SELECT payload FROM class_relations WHERE projection_id=?1 ORDER BY id")?
                .query_map([&class_id], |r| r.get::<_, String>(0))?
                .map(|payload| Ok(serde_json::from_str(&payload?)?))
                .collect::<Result<_>>()?;
            expected_relations.sort_by(|a, b| a.id.cmp(&b.id));
            actual_relations.sort_by(|a, b| a.id.cmp(&b.id));
            ensure!(
                actual_relations == expected_relations,
                "incompatible_index: selected class relationships differ from stored F catalog"
            );
        }
        Ok(())
    }

    pub fn native_declarations_at(
        &self,
        pin: IndexPin,
        language: &str,
        lookup_key: &str,
    ) -> Result<Vec<crate::native_evidence::Declaration>> {
        self.native_at(pin, |db, selected| {
            let mut paths=db.prepare(
                "SELECT DISTINCT m.path FROM native_version_declarations d JOIN revision_documents m
                 ON m.revision_id=?3 AND m.document_version_id=d.version_id
                 WHERE m.language=?1 AND d.lookup_key=?2 ORDER BY m.path LIMIT 5001"
            )?.query_map(params![language,lookup_key,selected.key],|r|r.get::<_,String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ensure!(
                paths.len() <= 5000,
                "incompatible_index: selected lookup document budget exceeded"
            );
            for path in paths.drain(..) {
                self.attest_selected_document_for(db, &path, selected)?;
            }
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
        let mut stmt=db.prepare("SELECT DISTINCT m.source_set_id,m.language,m.path
            FROM revision_documents m JOIN native_version_declarations d ON d.version_id=m.document_version_id
            WHERE m.revision_id=?5 AND m.language=?1
              AND (?4 OR ((?2 IS NULL AND d.lookup_key IS NULL) OR d.lookup_key=?2))
              AND (?3 IS NULL OR m.path=?3) ORDER BY m.path LIMIT 5001")?;
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
            let scope = v8_native_scope_for(db, &key, selected)?
                .context("incompatible_index: lookup manifest missing")?;
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
            self.attest_selected_document_for(db, &key.path, selected)?;
            let source = self
                .selected_source_row_for(db, &key.path, selected)?
                .context("incompatible_index: paired graph source missing")?;
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
            let scope = v8_native_scope_for(db, key, selected)?;
            if scope.is_some() {
                self.attest_selected_document_for(db, &key.path, selected)?;
            }
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

    pub fn native_calls_at(
        &self,
        pin: IndexPin,
        owner: &str,
    ) -> Result<Vec<crate::native_evidence::Call>> {
        self.native_at(pin, |db, selected| {
            if let Some(scope) = v8_owner_scope_for(db, owner, selected)? {
                self.attest_selected_document_for(db, &scope.key.path, selected)?;
                read_native_calls_v8(db, &scope, Some(owner))
            } else {
                let dangling: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_version_calls c JOIN revision_documents m ON m.document_version_id=c.version_id AND m.revision_id=?2 WHERE c.owner_syntax_id=?1)",
                    params![owner, selected.key],
                    |r| r.get(0),
                )?;
                ensure!(!dangling, "incompatible_index: native owner absent");
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
                self.attest_selected_document_for(db,&scope.key.path,selected)?;
                read_native_control_regions_v8(db,&scope,Some(owner))
            } else {
                let dangling:bool=db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM native_version_control_regions c JOIN revision_documents m ON m.document_version_id=c.version_id AND m.revision_id=?2 WHERE c.owner_syntax_id=?1)",
                    params![owner, selected.key],|r|r.get(0),
                )?;
                ensure!(!dangling,"incompatible_index: native owner absent");
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
        request.validate()?;
        self.with_evidence(|tx| self.navigation_in(tx, request, false))
    }
    fn navigation_in(
        &self,
        tx: &Connection,
        request: &crate::navigation::NavigationRequest,
        pre_startup: bool,
    ) -> Result<crate::navigation::NavigationResult> {
        let selected =
            self.read_revision_for(tx, Some(request.expected_revision()), pre_startup)?;
        let revision = selected.pin;
        // Navigation's source selector counts lines from the stored source BLOB,
        // and its member selector reads versioned class/node rows. Before either
        // consumes a selected document, authenticate it against that source BLOB
        // in this same read transaction. Never scan the entire workspace here.
        let selected_path: Option<String> = match request {
            crate::navigation::NavigationRequest::Source(s) => Some(s.path.clone()),
            crate::navigation::NavigationRequest::Member(s) => {
                let selected: Option<(String, bool, Option<String>)> = tx
                    .query_row(
                        "SELECT n.path,json_valid(n.payload),CASE WHEN json_valid(n.payload) THEN json_extract(n.payload,'$.kind') ELSE NULL END FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id WHERE n.id=?1 LIMIT 1",
                        params![s.class_id, selected.key],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(|error| self.report_selected_failure(error.into()))?;
                match selected {
                    Some((_path, false, _)) => {
                        return Err(self.report_selected_failure(
                            SelectedIntegrity(
                                "incompatible_index: selected navigation node JSON invalid".into(),
                            )
                            .into(),
                        ));
                    }
                    Some((path, true, Some(kind))) if kind == "class" => Some(path),
                    _ => None,
                }
            }
        };
        if let Some(path) = selected_path {
            // Gate allocation of the selected JSON/BLOB before decoding either.
            // This reads only SQLite byte lengths, not every workspace document.
            let sizes: Option<(i64, i64)> = tx
                .query_row(
                    "SELECT length(d.source_bytes),length(d.source_bytes) FROM revision_documents m
                 JOIN document_versions d ON d.id=m.document_version_id
                 WHERE m.revision_id=?2 AND m.path=?1",
                    params![path, selected.key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((graph_len, native_len)) = sizes {
                if graph_len > 2 * 1024 * 1024 || native_len > 2 * 1024 * 1024 {
                    return Err(self.report_selected_failure(
                        SelectedIntegrity(
                            "incompatible_index: selected navigation source exceeds budget".into(),
                        )
                        .into(),
                    ));
                }
                self.attest_selected_document_for(tx, &path, &selected)?;
            } else {
                let graph_file: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?2 AND path=?1)",
                    params![path, selected.key],
                    |row| row.get(0),
                )?;
                if graph_file {
                    return Err(self.report_selected_failure(
                        SelectedIntegrity(
                            "incompatible_index: selected native document missing".into(),
                        )
                        .into(),
                    ));
                }
            }
        }
        crate::navigation::navigate(tx, request, revision, &selected.key)
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
        let (mut warnings, truncated) = class_metadata(tx, &selected_revision)
            .map_err(|error| self.report_selected_failure(selected_integrity(error)))?;
        let pattern = format!(
            "%{}%",
            query
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        let mut stmt = tx.prepare("SELECT c.id FROM classes c JOIN revision_documents m ON m.revision_id=?6 AND m.class_projection_id=c.projection_id WHERE (?1 IS NULL OR c.path=?1)
            AND (c.name LIKE ?2 ESCAPE '\\' OR c.qualified_name LIKE ?2 ESCAPE '\\' OR c.id=?3)
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
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| self.report_selected_failure(error.into()))?;
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
        let mut paths: BTreeSet<_> = items
            .iter()
            .map(|class| class.symbol.path.as_str())
            .collect();
        if let Some(selected) = path
            && matches!(selected.rsplit('.').next(), Some("java" | "py"))
        {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM revision_documents WHERE revision_id=?2 AND path=?1)",
                params![selected, selected_revision.key],
                |r| r.get(0),
            )?;
            if exists {
                paths.insert(selected);
            }
        }
        self.attest_selected_classes_for(tx, &paths, &selected_revision)?;
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
        let (warnings, truncated) = class_metadata(tx, &selected)
            .map_err(|error| self.report_selected_failure(selected_integrity(error)))?;
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
        let paths: BTreeSet<_> = classes
            .values()
            .map(|class| class.symbol.path.as_str())
            .collect();
        self.attest_selected_classes_for(tx, &paths, &selected)?;
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
        let selected_paths: BTreeSet<&str> = rows.iter().map(|(path, _)| path.as_str()).collect();
        for path in &selected_paths {
            self.attest_selected_document_for(tx, path, &selected)?;
        }
        let values = rows
            .iter()
            .map(|(_, payload)| Ok(serde_json::from_str(payload)?))
            .collect::<Result<Vec<Symbol>>>()?;
        let paths: BTreeSet<_> = values.iter().map(|node| node.path.as_str()).collect();
        for path in paths.difference(&selected_paths) {
            self.attest_selected_document_for(tx, path, &selected)?;
        }
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
                .query_row("SELECT n.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id WHERE n.id=?1", params![id, selected.key], |r| r.get(0))
                .optional()?;
        if let Some(path) = selected_path {
            self.attest_selected_document_for(tx, &path, &selected)?;
        }
        let node: Option<Symbol> = one_at(
            tx,
            "SELECT n.payload FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id WHERE n.id=?1",
            id,
            &selected.key,
        )?;
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
             WHERE id=(SELECT document_version_id FROM revision_documents
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
            "SELECT document_version_id,source_set_id,language FROM revision_documents
             WHERE revision_id=?1 AND path=?2 LIMIT 2",
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
            "store_unavailable: ambiguous selected source path"
        );
        let Some((version, source_set, language)) = manifest else {
            return Ok(None);
        };
        let row: Option<(String, String, String, String, i64, Vec<u8>)> = db
            .query_row(
                "SELECT source_set_id,language,path,content_hash,byte_length,source_bytes
                 FROM document_versions WHERE id=?1",
                [&version],
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
            row.context("store_unavailable: selected source version absent")?;
        use sha2::{Digest, Sha256};
        ensure!(
            stored_set == source_set && stored_language == language && stored_path == path,
            "store_unavailable: selected raw source identity mismatch"
        );
        // Stored bytes that disagree with their own recorded length or hash are
        // corrupt: schedule a rebuild. This never covers decoding, so invalid
        // UTF-8 outside a requested range can't trigger it (W5).
        if usize::try_from(length).ok() != Some(bytes.len())
            || hash != hex::encode(Sha256::digest(&bytes))
        {
            self.mark_recovery(RecoveryDisposition::Rebuild);
            anyhow::bail!("store_unavailable: selected raw source length or digest mismatch");
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
            .map_err(selected_integrity)
            .map_err(|error| self.report_selected_failure(error))
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
        let selected_sql = "FROM revision_documents m JOIN document_versions d
            ON d.id=m.document_version_id AND d.source_set_id=m.source_set_id
              AND d.language=m.language AND d.path=m.path
            WHERE m.revision_id=?2 AND m.path=?1";
        let sql = format!(
            "SELECT length(d.source_bytes),length(CAST(d.source_set_id AS BLOB)),length(CAST(d.language AS BLOB)),length(CAST(d.path AS BLOB)),length(CAST(d.content_hash AS BLOB)) {selected_sql} LIMIT 2"
        );
        let mut stmt = db.prepare(&sql)?;
        let mut sizes = stmt.query(params![path, selected.key])?;
        let mut present = false;
        while let Some(row) = sizes.next()? {
            optional_hint_check(progress)?;
            ensure!(!present, "incompatible_index: ambiguous manifest source");
            present = true;
            let bytes: i64 = row.get(0)?;
            ensure!(
                bytes >= 0
                    && bytes <= max_bytes
                    && (1usize..5).all(|i| -> bool {
                        row.get::<_, i64>(i)
                            .is_ok_and(|length| (0..=16 * 1024).contains(&length))
                    }),
                "incompatible_index: selected source byte budget exceeded"
            );
        }
        if !present {
            return Ok(None);
        }
        let sql = format!(
            "SELECT d.source_set_id,d.language,d.path,d.content_hash,d.byte_length,d.source_bytes {selected_sql}"
        );
        let checked: Result<(String, String, String, String, Vec<u8>)> =
            db.query_row(&sql, params![path, selected.key], |r| {
                Ok((|| -> Result<_> {
                    use sha2::{Digest, Sha256};
                    let source_set: String = r.get(0)?;
                    let language: String = r.get(1)?;
                    let stored_path: String = r.get(2)?;
                    let hash: String = r.get(3)?;
                    let length: i64 = r.get(4)?;
                    let raw = r.get_ref(5)?.as_blob()?;
                    let mut digest = Sha256::new();
                    for (index, chunk) in raw.chunks(64 * 1024).enumerate() {
                        if index == 0 && progress.is_some() {
                            #[cfg(feature = "test-support")]
                            crate::mcp::phase_hook("outgoing_hint_source_hash_chunk")
                                .map_err(|error| anyhow::anyhow!(error))?;
                        }
                        optional_hint_check(progress)?;
                        digest.update(chunk);
                    }
                    optional_hint_check(progress)?;
                    ensure!(
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
            text: String::from_utf8(bytes)?,
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
            if source.is_some() {
                self.attest_selected_document_for(tx, path, &selected)?;
            }
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
            let mut valid_stmt = tx.prepare(
                "SELECT NOT EXISTS(SELECT 1 FROM graph_nodes n WHERE n.projection_id=m.graph_projection_id AND json_valid(n.payload)=0) FROM revision_documents m JOIN index_metadata active_manifest ON active_manifest.singleton=1 AND m.revision_id='pin:v1:'||active_manifest.index_generation||':'||active_manifest.index_revision WHERE m.path=?1",
            )?;
            let mut count_stmt = tx.prepare(
                "SELECT (SELECT count(*) FROM graph_nodes n WHERE n.projection_id=m.graph_projection_id AND CASE WHEN json_valid(n.payload) THEN json_extract(n.payload,'$.kind') IN ('function','method') ELSE 0 END) FROM revision_documents m JOIN index_metadata active_manifest ON active_manifest.singleton=1 AND m.revision_id='pin:v1:'||active_manifest.index_generation||':'||active_manifest.index_revision WHERE m.path=?1",
            )?;
            let mut overlays = Vec::new();
            for (index, item) in items.iter().enumerate().filter(|(_, entry)| entry.kind == "file") {
                let absolute = root.join(&item.path);
                let Ok(relative) = absolute.strip_prefix(workspace) else { continue };
                let Some(relative) = relative.to_str() else { continue };
                let valid: Option<bool> = valid_stmt
                    .query_row([relative], |row| row.get(0))
                    .optional()
                    .map_err(|error| self.report_selected_failure(error.into()))?;
                let Some(valid) = valid else { continue };
                if !valid {
                    return Err(self.report_selected_failure(
                        SelectedIntegrity("incompatible_index: selected tree node JSON invalid".into()).into(),
                    ));
                }
                let count: i64 = count_stmt
                    .query_row([relative], |row| row.get(0))
                    .map_err(|error| self.report_selected_failure(error.into()))?;
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
        let mut valid_stmt = tx.prepare(
                "SELECT NOT EXISTS(SELECT 1 FROM graph_nodes n WHERE n.projection_id=m.graph_projection_id AND json_valid(n.payload)=0) FROM revision_documents m JOIN index_metadata active_manifest ON active_manifest.singleton=1 AND m.revision_id='pin:v1:'||active_manifest.index_generation||':'||active_manifest.index_revision WHERE m.path=?1",
            )?;
        let mut count_stmt = tx.prepare(
                "SELECT (SELECT count(*) FROM graph_nodes n WHERE n.projection_id=m.graph_projection_id AND CASE WHEN json_valid(n.payload) THEN json_extract(n.payload,'$.kind') IN ('function','method') ELSE 0 END) FROM revision_documents m JOIN index_metadata active_manifest ON active_manifest.singleton=1 AND m.revision_id='pin:v1:'||active_manifest.index_generation||':'||active_manifest.index_revision WHERE m.path=?1",
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
            let valid: Option<bool> = valid_stmt
                .query_row([relative], |row| row.get(0))
                .optional()
                .map_err(|error| self.report_selected_failure(error.into()))?;
            let Some(valid) = valid else { continue };
            if !valid {
                return Err(self.report_selected_failure(
                    SelectedIntegrity("incompatible_index: selected tree node JSON invalid".into())
                        .into(),
                ));
            }
            let count: i64 = count_stmt
                .query_row([relative], |row| row.get(0))
                .map_err(|error| self.report_selected_failure(error.into()))?;
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
            "SELECT m.path,d.language,m.graph_projection_id FROM revision_documents m
                 JOIN document_versions d ON d.id=m.document_version_id
                 WHERE m.revision_id=?3 ORDER BY m.path LIMIT ?1 OFFSET ?2",
        )?;
        let source_rows: Vec<(String, String, String)> = stmt
            .query_map(
                params![(limit + 1) as i64, offset as i64, selected.key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?
            .collect::<rusqlite::Result<_>>()?;
        // The extra row only decides whether a next page exists; it isn't
        // returned, so only the returned rows are attested and counted.
        let next = (source_rows.len() > limit).then_some(offset + limit);
        let mut items = Vec::new();
        for (path, language, projection_id) in source_rows.into_iter().take(limit) {
            self.attest_selected_document_for(tx, &path, &selected)?;
            let methods: i64 = tx.query_row(
                "SELECT count(*) FROM graph_nodes WHERE projection_id=?1
                    AND json_extract(payload,'$.kind') IN ('function','method')",
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
        let projection_id: Option<String> = tx
            .query_row(
                "SELECT m.graph_projection_id FROM revision_documents m
                     WHERE m.revision_id=?2 AND m.path=?1",
                params![path, selected.key],
                |r| r.get(0),
            )
            .optional()?;
        let Some(projection_id) = projection_id else {
            return Ok(None);
        };
        self.attest_selected_document_for(tx, path, &selected)?;
        let mut stmt = tx.prepare(
            "SELECT payload FROM graph_nodes WHERE projection_id=?1
                AND json_extract(payload,'$.kind') IN ('function','method')
                ORDER BY json_extract(payload,'$.range.startByte'),id LIMIT 1001",
        )?;
        let mut items = Vec::new();
        for payload in stmt.query_map([&projection_id], |r| r.get::<_, String>(0))? {
            let symbol: Symbol = serde_json::from_str(&payload?)?;
            items.push(serde_json::json!({"symbol":symbol,"consequential":true,
                    "reason":"Conservative heuristic: retained; triviality is not proven"}));
        }
        let truncated = items.len() > 1000;
        items.truncate(1000);
        Ok(Some(
            serde_json::json!({"revision":revision,"items":items,"truncated":truncated}),
        ))
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
        let selected: Option<(String, String)> = tx
            .query_row(
                "SELECT n.path,n.projection_id FROM graph_nodes n JOIN revision_documents m
                 ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id
                 WHERE n.id=?1",
                params![seed, selected_revision.key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((path, graph_id)) = selected else {
            return Ok(None);
        };
        self.attest_selected_document_for(tx, &path, &selected_revision)?;
        let payload: String = tx.query_row(
            "SELECT payload FROM graph_nodes WHERE projection_id=?1 AND id=?2",
            params![graph_id, seed],
            |r| r.get(0),
        )?;
        let symbol: Symbol = serde_json::from_str(&payload)?;
        ensure!(
            matches!(symbol.kind, SymbolKind::Function | SymbolKind::Method),
            "invalid sequence symbol kind"
        );
        let file = self
            .selected_source_row_for(tx, &path, &selected_revision)?
            .context("sequence source missing")?;
        let mut stmt = tx.prepare(
            "SELECT payload FROM graph_calls WHERE projection_id=?1
                ORDER BY json_extract(payload,'$.range.startByte'),id",
        )?;
        let calls: Vec<CallSite> = stmt
            .query_map([&graph_id], |r| r.get::<_, String>(0))?
            .map(|payload| Ok(serde_json::from_str(&payload?)?))
            .collect::<Result<_>>()?;
        crate::behavior::build_sequence(revision, &symbol, &file, &calls, show_all).map(Some)
    }

    fn read_graph_for(&self, db: &Connection, selected: &ReadRevision) -> Result<Graph> {
        let (stats_type, stats_len, diagnostics_type, diagnostics_len): (String, i64, String, i64) =
            db.query_row(
                "SELECT typeof(graph_stats),length(CAST(graph_stats AS BLOB)),
                typeof(graph_diagnostics),length(CAST(graph_diagnostics AS BLOB))
             FROM native_revisions WHERE id=?1",
                [&selected.key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|error| self.report_selected_failure(error.into()))?;
        ensure!(
            stats_type == "text"
                && diagnostics_type == "text"
                && (0..=GRAPH_FIELD_MAX_BYTES).contains(&stats_len)
                && (0..=GRAPH_FIELD_MAX_BYTES).contains(&diagnostics_len)
                && stats_len + diagnostics_len <= GRAPH_PAIR_MAX_BYTES,
            "incompatible_index: retained graph metadata byte budget exceeded"
        );
        let (raw_stats, raw_diagnostics): (String, String) = db
            .query_row(
                "SELECT graph_stats,graph_diagnostics FROM native_revisions WHERE id=?1",
                [&selected.key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|error| self.report_selected_failure(error.into()))?;
        let stats: IndexStats = serde_json::from_str(&raw_stats)
            .map_err(|error| self.report_selected_failure(selected_integrity(error.into())))?;
        let diagnostics: Vec<Diagnostic> = serde_json::from_str(&raw_diagnostics)
            .map_err(|error| self.report_selected_failure(selected_integrity(error.into())))?;
        let paths: Vec<String> = db
            .prepare(
                "SELECT m.path FROM revision_documents m WHERE m.revision_id=?1 ORDER BY m.path",
            )?
            .query_map([&selected.key], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for path in &paths {
            self.attest_selected_document_for(db, path, selected)?;
        }
        let files = paths
            .iter()
            .map(|path| {
                self.selected_source_row_for(db, path, selected)?
                    .context("missing manifest graph source")
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Graph {
            schema_version: SCHEMA_VERSION,
            files,
            nodes: rows_at(db, "SELECT n.payload FROM graph_nodes n JOIN revision_documents d
                ON d.revision_id=?1 AND d.graph_projection_id=n.projection_id ORDER BY n.id", &selected.key)?,
            calls: rows_at(db, "SELECT c.payload FROM graph_calls c JOIN revision_documents d
                ON d.revision_id=?1 AND d.graph_projection_id=c.projection_id
                ORDER BY c.path,json_extract(c.payload,'$.range.startByte'),json_extract(c.payload,'$.range.endByte') DESC,c.id", &selected.key)?,
            regions: rows_at(db, "SELECT g.payload FROM graph_regions g JOIN revision_documents d
                ON d.revision_id=?1 AND d.graph_projection_id=g.projection_id ORDER BY g.id", &selected.key)?,
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

    /// Simulate accidental same-pin damage while a test provider is paused. Use
    /// the protected Store connection, not a second raw SQLite file handle.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn corrupt_selected_source_for_tests(
        &self,
        pin: IndexPin,
        path: &str,
    ) -> Result<()> {
        let mut db = self.cache_write()?;
        let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let selected = self.read_revision_for(&tx, Some(pin), true)?;
        let changed = storage_result(tx.execute(
            "UPDATE document_versions SET source_bytes=zeroblob(length(source_bytes))              WHERE id=(SELECT m.document_version_id FROM revision_documents m              WHERE m.revision_id=?1 AND m.path=?2) AND length(source_bytes)>0",
            params![selected.key, path],
        ))?;
        ensure!(changed == 1, "test fixture: selected source row missing");
        self.identity.verify()?;
        storage_result(tx.commit())
    }

    /// Reauthenticate only a cached packet's selected evidence under one pin.
    /// Unrelated documents are not read; status still validates persisted capture inventory.
    pub fn validate_selected_view(&self, view: &ViewResult, sources: &[SourceFile]) -> Result<()> {
        self.with_evidence(|tx| self.validate_selected_view_in(tx, view, sources))
    }
    fn validate_selected_view_in(
        &self,
        tx: &Connection,
        view: &ViewResult,
        sources: &[SourceFile],
    ) -> Result<()> {
        let selected = self.read_revision(tx, Some(view.revision))?;
        let paths: BTreeSet<_> = view
            .nodes
            .iter()
            .map(|n| n.path.as_str())
            .chain(view.calls.iter().map(|c| c.path.as_str()))
            .chain(view.regions.iter().map(|r| r.path.as_str()))
            .chain(sources.iter().map(|f| f.path.as_str()))
            .collect();
        for path in paths {
            self.attest_selected_document_for(tx, path, &selected)?;
        }
        for node in &view.nodes {
            let actual: Option<Symbol> = one_at(
                tx,
                "SELECT n.payload FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id WHERE n.id=?1",
                &node.id,
                &selected.key,
            )?;
            ensure!(
                actual.as_ref() == Some(node),
                "incompatible_index: cached packet selected graph declaration changed"
            );
        }
        for call in &view.calls {
            let actual: Option<CallSite> = one_at(
                tx,
                "SELECT c.payload FROM graph_calls c JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=c.projection_id WHERE c.id=?1",
                &call.id,
                &selected.key,
            )?;
            ensure!(
                actual.as_ref() == Some(call),
                "incompatible_index: cached packet selected graph call changed"
            );
        }
        for region in &view.regions {
            let actual: Option<ControlRegion> = one_at(
                tx,
                "SELECT g.payload FROM graph_regions g JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=g.projection_id WHERE g.id=?1",
                &region.id,
                &selected.key,
            )?;
            ensure!(
                actual.as_ref() == Some(region),
                "incompatible_index: cached packet selected graph region changed"
            );
        }
        for source in sources {
            ensure!(
                self.selected_source_row_for(tx, &source.path, &selected)?
                    .as_ref()
                    == Some(source),
                "incompatible_index: cached packet selected source changed"
            );
        }
        Ok(())
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
            .query_row("SELECT n.path FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id WHERE n.id=?1", params![query.seed, selected.key], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(path) = selected_path {
            self.attest_selected_document_for(tx, &path, &selected)?;
        }
        let seed: Option<Symbol> = one_at(
            tx,
            "SELECT n.payload FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id WHERE n.id=?1",
            &query.seed,
            &selected.key,
        )?;
        let Some(seed) = seed else { return Ok(None) };
        let mut calls = Vec::new();
        let mut region_ids = BTreeSet::new();
        let mut truncated = false;
        if !query.exclude_paths.iter().any(|p| seed.path.starts_with(p)) {
            let mut stmt = tx.prepare("SELECT c.payload FROM graph_calls c JOIN revision_documents m ON m.revision_id=?3 AND m.graph_projection_id=c.projection_id WHERE c.caller=?1 ORDER BY c.path,json_extract(c.payload,'$.range.startByte'),json_extract(c.payload,'$.range.endByte') DESC,c.id LIMIT ?2")?;
            let rows = stmt.query_map(
                params![seed.id, (query.max_calls + 1) as i64, selected.key],
                |r| r.get::<_, String>(0),
            )?;
            for payload in rows {
                let call: CallSite = serde_json::from_str(&payload?)?;
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
            let region: Option<ControlRegion> = one_at(
                tx,
                "SELECT g.payload FROM graph_regions g JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=g.projection_id WHERE g.id=?1",
                &id,
                &selected.key,
            )?;
            if let Some(region) = region {
                if let Some(parent) = &region.parent {
                    region_ids.insert(parent.clone());
                }
                regions.insert(id, region);
            }
        }
        let paths: BTreeSet<_> = std::iter::once(seed.path.as_str())
            .chain(calls.iter().map(|c| c.path.as_str()))
            .chain(regions.values().map(|r| r.path.as_str()))
            .collect();
        for path in paths {
            self.attest_selected_document_for(tx, path, &selected)?;
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

    pub fn put_view(&self, view: &SavedView) -> Result<()> {
        view.validate()?;
        self.records().put_view(view)?;
        Ok(())
    }

    fn selected_anchor_in(
        db: &Connection,
        store: &Self,
        target: &str,
        pre_startup: bool,
    ) -> Result<DurableAnchor> {
        let revision = store.read_revision_for(db, None, pre_startup)?;
        let selected: Option<(String, String)> = db
            .query_row(
                "SELECT m.language,m.path FROM native_version_declarations d
                 JOIN revision_documents m ON m.revision_id=?2 AND m.document_version_id=d.version_id
                 WHERE d.syntax_id=?1",
                params![target, revision.key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (language, path) = selected.context("native declaration target missing")?;
        store.attest_selected_document_for(db, &path, &revision)?;
        let declarations =
            Self::read_native_declarations_for(db, &language, None, Some(&path), true, &revision)?;
        let focused = anchors::find_selected(target, &declarations)?;
        anchors::capture_anchor(focused, &declarations)
    }

    fn declarations_for_anchor(
        db: &Connection,
        store: &Self,
        anchor: &DurableAnchor,
        selected: &ReadRevision,
    ) -> Result<Option<(String, Vec<crate::native_evidence::Declaration>)>> {
        let revision: Option<String> = db
            .query_row(
                "SELECT r.native_revision_id FROM revision_documents m
             JOIN native_revisions r ON r.id=m.revision_id
             WHERE m.revision_id=?4 AND m.source_set_id=?1 AND m.language=?2 AND m.path=?3",
                params![
                    anchor.document.source_set_id,
                    anchor.document.language,
                    anchor.document.path,
                    selected.key
                ],
                |row| row.get(0),
            )
            .optional()?;
        let Some(revision) = revision else {
            return Ok(None);
        };
        store.attest_selected_document_for(db, &anchor.document.path, selected)?;
        let declarations = Self::read_native_declarations_for(
            db,
            &anchor.document.language,
            None,
            Some(&anchor.document.path),
            true,
            selected,
        )?;
        ensure!(
            anchors::document_matches(&anchor.document, &declarations),
            "invalid anchor document association"
        );
        Ok(Some((revision, declarations)))
    }

    fn anchor_attachment(
        db: &Connection,
        store: &Self,
        raw: Option<&serde_json::value::RawValue>,
        selected: &ReadRevision,
    ) -> Result<AnchorAttachment> {
        let Some(raw) = raw else {
            return Ok(AnchorAttachment {
                availability: AttachmentAvailability::Anchorless,
                result: None,
            });
        };
        let anchor: DurableAnchor = serde_json::from_str(raw.get())?;
        anchor.validate()?;
        let result = match Self::declarations_for_anchor(db, store, &anchor, selected)? {
            Some((revision, declarations)) => {
                let current = declarations
                    .iter()
                    .find(|row| row.syntax_id == anchor.syntax_id);
                anchors::audit_anchor(&anchor, &revision, current, &declarations, None)?
            }
            None => AnchorResult::orphaned(AnchorReason::Missing),
        };
        Ok(AnchorAttachment {
            availability: AttachmentAvailability::Ready,
            result: Some(result),
        })
    }

    fn resolve_view(
        db: &Connection,
        store: &Self,
        view: SavedViewRecord,
        pin: Option<IndexPin>,
        pre_startup: bool,
    ) -> Result<SavedViewState> {
        let selected = store.read_revision_for(db, pin, pre_startup)?;
        let ids: BTreeSet<&String> = std::iter::once(&view.query.seed)
            .chain(view.pins.keys())
            .chain(view.hidden.iter())
            .collect();
        let mut orphaned_ids = vec![];
        for id in ids {
            let exists: bool = db.query_row(
                "SELECT EXISTS(SELECT 1 FROM graph_nodes n JOIN revision_documents m ON m.revision_id=?2 AND m.graph_projection_id=n.projection_id WHERE n.id=?1)",
                params![id, selected.key],
                |r| r.get(0),
            )?;
            if !exists {
                orphaned_ids.push(id.clone());
            }
        }
        let attachment = Self::anchor_attachment(db, store, view.anchor.as_deref(), &selected)?;
        if attachment
            .result
            .as_ref()
            .is_none_or(|result| result.status != AnchorStatus::Attached)
            && !orphaned_ids.contains(&view.query.seed)
        {
            orphaned_ids.push(view.query.seed.clone());
        }
        Ok(SavedViewState {
            view,
            orphaned_ids,
            index_generation: pin.map(|p| p.index_generation.to_string()),
            index_revision: pin.map(|p| p.index_revision),
            attachment,
        })
    }
    // Durable records are not index evidence. Admit even an empty result before returning it.
    fn saved_pin(response: &EvidenceResponse, expected_pin: Option<IndexPin>) -> Result<IndexPin> {
        Ok(response
            .store
            .read_revision(&response.db, expected_pin)?
            .pin)
    }

    fn finish_saved<T>(&self, response: &EvidenceResponse, value: T) -> Result<T> {
        // A publication in flight must not turn an old attachment into a Ready response.
        self.ensure_public_read_ready()?;
        response.finish(value)
    }

    fn unavailable_view(view: SavedViewRecord) -> SavedViewState {
        let orphaned_ids = std::iter::once(view.query.seed.clone())
            .chain(view.pins.keys().cloned())
            .chain(view.hidden.iter().cloned())
            .collect();
        SavedViewState {
            view,
            orphaned_ids,
            index_generation: None,
            index_revision: None,
            attachment: AnchorAttachment {
                availability: AttachmentAvailability::IndexUnavailable,
                result: None,
            },
        }
    }

    pub fn saved_views_at(&self, expected_pin: Option<IndexPin>) -> Result<Vec<SavedViewState>> {
        let views = self.records().view_records()?;
        let response = match self.evidence_response() {
            Ok(response) => response,
            Err(error) if expected_pin.is_none() && native_index_unavailable(&error) => {
                return Ok(views.into_iter().map(Self::unavailable_view).collect());
            }
            Err(error) => return Err(error),
        };
        let pin = Self::saved_pin(&response, expected_pin)?;
        let states = views
            .into_iter()
            .map(|view| Self::resolve_view(&response.db, self, view, Some(pin), false))
            .collect::<Result<Vec<_>>>()?;
        self.finish_saved(&response, states)
    }
    pub fn views(&self) -> Result<Vec<SavedViewState>> {
        self.saved_views_at(None)
    }

    pub fn saved_view_at(
        &self,
        id: &str,
        expected_pin: Option<IndexPin>,
    ) -> Result<Option<SavedViewState>> {
        let view = self.records().view_record(id)?;
        let response = match self.evidence_response() {
            Ok(response) => response,
            Err(error) if expected_pin.is_none() && native_index_unavailable(&error) => {
                return Ok(view.map(Self::unavailable_view));
            }
            Err(error) => return Err(error),
        };
        let pin = Self::saved_pin(&response, expected_pin)?;
        let state = view
            .map(|view| Self::resolve_view(&response.db, self, view, Some(pin), false))
            .transpose()?;
        self.finish_saved(&response, state)
    }
    pub fn view(&self, id: &str) -> Result<Option<SavedViewState>> {
        self.saved_view_at(id, None)
    }

    pub fn save_view_at(&self, pin: IndexPin, view: &SavedView) -> Result<SavedViewState> {
        view.validate()?;
        let response = self.evidence_response()?;
        Self::saved_pin(&response, Some(pin))?;
        ensure!(
            response.status()?.revision == pin,
            "revision conflict: mutation requires head"
        );
        let record = self.records().update_view_record(
            &SavedViewRecord::from_base(view.clone(), None),
            || {
                serde_json::value::to_raw_value(&Self::selected_anchor_in(
                    &response.db,
                    self,
                    &view.query.seed,
                    false,
                )?)
                .map_err(Into::into)
            },
        )?;
        let state = Self::resolve_view(&response.db, self, record, Some(pin), false)?;
        self.finish_saved(&response, state)
    }
    pub fn delete_view(&self, id: &str) -> Result<bool> {
        self.records().delete_view(id)
    }

    pub fn put_annotation(&self, annotation: &Annotation) -> Result<()> {
        annotation.validate()?;
        self.records().put_annotation(annotation)?;
        Ok(())
    }
    fn resolve_annotation(
        db: &Connection,
        store: &Self,
        annotation: AnnotationRecord,
        pin: Option<IndexPin>,
        pre_startup: bool,
    ) -> Result<AnnotationState> {
        let selected = store.read_revision_for(db, pin, pre_startup)?;
        let attachment =
            Self::anchor_attachment(db, store, annotation.anchor.as_deref(), &selected)?;
        let orphaned = attachment
            .result
            .as_ref()
            .is_none_or(|result| result.status != AnchorStatus::Attached);
        Ok(AnnotationState {
            annotation,
            orphaned,
            index_generation: pin.map(|p| p.index_generation.to_string()),
            index_revision: pin.map(|p| p.index_revision),
            attachment,
        })
    }
    fn unavailable_annotation(annotation: AnnotationRecord) -> AnnotationState {
        AnnotationState {
            annotation,
            orphaned: true,
            index_generation: None,
            index_revision: None,
            attachment: AnchorAttachment {
                availability: AttachmentAvailability::IndexUnavailable,
                result: None,
            },
        }
    }
    pub fn saved_annotations_at(
        &self,
        expected_pin: Option<IndexPin>,
    ) -> Result<Vec<AnnotationState>> {
        let annotations = self.records().annotation_records()?;
        let response = match self.evidence_response() {
            Ok(response) => response,
            Err(error) if expected_pin.is_none() && native_index_unavailable(&error) => {
                return Ok(annotations
                    .into_iter()
                    .map(Self::unavailable_annotation)
                    .collect());
            }
            Err(error) => return Err(error),
        };
        let pin = Self::saved_pin(&response, expected_pin)?;
        let states = annotations
            .into_iter()
            .map(|item| Self::resolve_annotation(&response.db, self, item, Some(pin), false))
            .collect::<Result<Vec<_>>>()?;
        self.finish_saved(&response, states)
    }
    pub fn annotations(&self) -> Result<Vec<AnnotationState>> {
        self.saved_annotations_at(None)
    }

    pub fn save_annotation_at(
        &self,
        pin: IndexPin,
        request: &AnnotationRequest,
    ) -> Result<AnnotationState> {
        request.validate()?;
        let response = self.evidence_response()?;
        Self::saved_pin(&response, Some(pin))?;
        ensure!(
            response.status()?.revision == pin,
            "revision conflict: mutation requires head"
        );
        let title = request
            .title
            .as_ref()
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let record = self.records().update_annotation_record(
            &AnnotationRecord::from_base(request.base(), title, None),
            request.title.is_none(),
            || {
                serde_json::value::to_raw_value(&Self::selected_anchor_in(
                    &response.db,
                    self,
                    &request.node_id,
                    false,
                )?)
                .map_err(Into::into)
            },
        )?;
        let state = Self::resolve_annotation(&response.db, self, record, Some(pin), false)?;
        self.finish_saved(&response, state)
    }
    pub fn delete_annotation(&self, id: &str) -> Result<bool> {
        self.records().delete_annotation(id)
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
mod metrics_parallel_tests;
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
mod preflight_hash_reuse_tests;
