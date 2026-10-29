//! Bounded read-only MCP protocol and single-connection stdio transport.
pub mod architecture;
pub mod catalog;
#[cfg(test)]
mod find_trigram_tests;
mod projection;
pub mod search;
pub mod session;
pub mod tools;
pub mod wire;

use crate::daemon::registry::SelectionError;
use crate::store::topology::WorkspaceIdentity;
use std::os::unix::fs::MetadataExt;

/// An already selected and marker-attached workspace. Never opens an index or repairs a marker.
#[derive(Debug)]
pub struct OpenedWorkspace {
    identity: WorkspaceIdentity,
    label: String,
    /// CALL_DEADLINE, or a shorter test-support deadline for this connection.
    call_deadline: std::time::Duration,
    #[cfg(feature = "test-support")]
    hooks: TransportHooksForTests,
}

/// A test hook given a call's JSON-RPC ID.
#[cfg(feature = "test-support")]
pub type CallHookForTests = std::sync::Arc<dyn Fn(&serde_json::Value) + Send + Sync>;

#[cfg(feature = "test-support")]
type EvidenceErrorHook =
    std::sync::Arc<dyn Fn(&serde_json::Value, &'static str, &str) + Send + Sync>;

/// Test-only pause points on one connection's transport.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct TransportHooksForTests {
    /// A paused call clock shared with this fixture's checkout runtime. Its
    /// presence disables real-time call-expiry wakeups; explicit input or worker
    /// completion publishes progress. Final-check retry timers stay real-time.
    pub paused_clock_for_tests:
        Option<std::sync::Arc<dyn Fn() -> std::time::Instant + Send + Sync>>,
    /// On the reader thread, after each frame is complete and counted, before
    /// it is published to the coordinator.
    pub frame_read: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// On the coordinator, when a decision waits for a frame completed before
    /// it but not yet published.
    pub settle_waiting: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// On the coordinator, before the fence that precedes a call's
    /// immediate decision (dispatch, validation or capacity); given its ID.
    pub immediate_decision: Option<CallHookForTests>,
    /// After each fence, with the number of requests awaiting decision.
    pub fence_handled: Option<std::sync::Arc<dyn Fn(usize) + Send + Sync>>,
    /// On a call worker, once its result has been handed to the coordinator.
    pub worker_done: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Opt-in diagnostic for a selected read/finish error. Never enters MCP output.
    pub evidence_error: Option<EvidenceErrorHook>,
    /// On the coordinator, at a finished call's send decision after its final
    /// root check, before the last input pass; given the call's ID.
    pub send_decision: Option<CallHookForTests>,
}
#[cfg(feature = "test-support")]
impl std::fmt::Debug for TransportHooksForTests {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransportHooksForTests")
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityError {
    RootChanged,
    StoreUnavailable,
}
impl OpenedWorkspace {
    pub fn new(identity: WorkspaceIdentity) -> Self {
        let label = identity
            .root
            .file_name()
            .map(|name| name.to_str().expect("canonical root is UTF-8").to_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "workspace".into());
        Self {
            identity,
            label,
            call_deadline: CALL_DEADLINE,
            #[cfg(feature = "test-support")]
            hooks: TransportHooksForTests::default(),
        }
    }
    /// Install transport pause points for this connection.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn with_transport_hooks_for_tests(mut self, hooks: TransportHooksForTests) -> Self {
        self.hooks = hooks;
        self
    }
    /// Shorten this connection's call deadline so a test exercises it
    /// without waiting real seconds.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn with_call_deadline_for_tests(mut self, deadline: std::time::Duration) -> Self {
        self.call_deadline = deadline;
        self
    }
    fn clock_now(&self) -> std::time::Instant {
        #[cfg(feature = "test-support")]
        if let Some(clock) = &self.hooks.paused_clock_for_tests {
            return clock();
        }
        std::time::Instant::now()
    }
    fn helper_deadline(&self, deadline: std::time::Instant) -> std::time::Instant {
        #[cfg(feature = "test-support")]
        if self.hooks.paused_clock_for_tests.is_some() {
            return std::time::Instant::now()
                + deadline.saturating_duration_since(self.clock_now());
        }
        deadline
    }
    fn call_timer_enabled(&self) -> bool {
        #[cfg(feature = "test-support")]
        if self.hooks.paused_clock_for_tests.is_some() {
            return false;
        }
        true
    }
    pub fn label(&self) -> &str {
        &self.label
    }
    pub fn root(&self) -> &std::path::Path {
        &self.identity.root
    }
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }
    pub fn build_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }
    pub fn check(&self) -> Result<(), IdentityError> {
        let m = std::fs::symlink_metadata(&self.identity.root)
            .map_err(|_| IdentityError::RootChanged)?;
        if !m.is_dir()
            || m.file_type().is_symlink()
            || (m.dev(), m.ino()) != (self.identity.device, self.identity.inode)
        {
            return Err(IdentityError::RootChanged);
        }
        self.identity
            .verify()
            .map_err(|_| IdentityError::StoreUnavailable)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn root_guard() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("selected");
        std::fs::create_dir(&root).unwrap();
        let context =
            OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), temp.path()).unwrap());
        assert_eq!(context.check(), Ok(()));
        assert_eq!(context.label(), "selected");
        std::fs::rename(&root, temp.path().join("former")).unwrap();
        assert_eq!(context.check(), Err(IdentityError::RootChanged));
        std::os::unix::fs::symlink(temp.path().join("former"), &root).unwrap();
        assert_eq!(context.check(), Err(IdentityError::RootChanged));
    }
    #[test]
    fn marker_guard_does_not_repair() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("git-workspace");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        let context =
            OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), temp.path()).unwrap());
        assert_eq!(context.check(), Ok(()));
        let marker = root.join(".git/trellis/workspace-id");
        std::fs::remove_file(&marker).unwrap();
        assert_eq!(context.check(), Err(IdentityError::StoreUnavailable));
        assert!(!marker.exists());
    }
}

/// Run the line-framed transport. The reader owns stdin; this coordinator alone owns stdout.
/// Input already waiting in the bounded channel is applied before any queued outcome commits.
pub fn run_stdio(workspace: OpenedWorkspace) -> std::io::Result<()> {
    run_transport(
        workspace,
        std::io::stdin(),
        std::io::stdout(),
        None,
        None,
        false,
    )
}

/// Run the same protocol coordinator on a daemon-owned session socket.
pub fn run_socket(
    workspace: OpenedWorkspace,
    stream: std::os::unix::net::UnixStream,
    registry: std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    session_id: u64,
    daemon_handle: tokio::runtime::Handle,
) -> std::io::Result<()> {
    let reader = stream.try_clone()?;
    run_transport(
        workspace,
        reader,
        stream,
        Some((registry, session_id)),
        Some(daemon_handle),
        false,
    )
}

/// The daemon side of `trellis mcp`'s relay (Decision 0009 §7): every frame
/// and reply carries the relay's own ID before the unchanged MCP line.
pub fn run_relayed_socket(
    workspace: OpenedWorkspace,
    stream: std::os::unix::net::UnixStream,
    registry: std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    session_id: u64,
    daemon_handle: tokio::runtime::Handle,
) -> std::io::Result<()> {
    let reader = stream.try_clone()?;
    run_transport(
        workspace,
        reader,
        stream,
        Some((registry, session_id)),
        Some(daemon_handle),
        true,
    )
}

/// Convert only an already validated wire basis; the model deserializer has a
/// narrower UUIDv4/plain-integer contract than the MCP wire schema.
fn validated_expected_basis(
    args: &serde_json::Map<String, serde_json::Value>,
) -> Option<crate::model::IndexPin> {
    let value = args.get("expectedBasis")?;
    Some(crate::model::IndexPin {
        index_generation: uuid::Uuid::parse_str(
            value["indexGeneration"]
                .as_str()
                .expect("validated generation"),
        )
        .expect("validated canonical UUID"),
        index_revision: wire::uint(&value["indexRevision"]).expect("validated revision"),
    })
}

fn mcp_read_error(error: &anyhow::Error) -> &'static str {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<crate::store::RebuildInProgress>()
            .is_some()
    }) {
        return "rebuild_in_progress";
    }
    if error
        .downcast_ref::<crate::store::topology::StorageBusy>()
        .is_some()
    {
        return "deadline_exceeded";
    }
    if error
        .downcast_ref::<crate::store::topology::IndexNotReady>()
        .is_some()
        || error.to_string().starts_with("index_not_ready:")
    {
        "index_not_ready"
    } else if error.to_string().starts_with("deadline_exceeded:") {
        "deadline_exceeded"
    } else if error.to_string().starts_with("root_changed:") {
        "root_changed"
    } else {
        "store_unavailable"
    }
}

/// A rooted call-graph contract failure's wire envelope. The closed failure-code set is shared
/// with every other tool, so the two `not_found` causes differ only by message.
fn call_graph_failure(
    id: &serde_json::Value,
    error: &crate::graph_query::GraphError,
) -> serde_json::Value {
    use crate::graph_query::GraphErrorCode::*;
    let (code, message) = match error.code {
        InvalidRequest => ("invalid_request", None),
        RootMissing => (
            "not_found",
            Some("Call graph root not found in the admitted snapshot"),
        ),
        ProducerUnavailable => (
            "not_found",
            Some("Semantic producer not published for the admitted revision"),
        ),
        InvalidRecord | InvalidRange | InvalidDigest | UnsupportedEncoding | SourceSetDenied
        | RevisionUnavailable => ("store_unavailable", None),
    };
    let mut envelope = tools::failure(id, code);
    if let Some(message) = message {
        envelope["error"]["message"] = serde_json::json!(message);
    }
    envelope
}

/// A failed evidence read's wire code.
fn evidence_failure_code(error: &anyhow::Error) -> &'static str {
    let message = error.to_string();
    if message.starts_with("revision conflict:") {
        "revision_conflict"
    } else if message.starts_with("unsupported_encoding:") {
        "unsupported_encoding"
    } else if message.starts_with("range_too_large:") {
        "range_too_large"
    } else if message.starts_with("invalid_request:") {
        "invalid_request"
    } else {
        mcp_read_error(error)
    }
}

/// One tool's read and projection inside an admitted snapshot. An error is
/// either retryable contention or a failure to report with its wire code.
type EvidenceToolReadFn<'a> = dyn Fn(
        &crate::daemon::registry::CheckoutRuntime,
        &crate::daemon::registry::CheckoutEvidence,
        bool,
        Option<&crate::daemon::search::SearchQuery>,
    ) -> anyhow::Result<serde_json::Value>
    + 'a;
type EvidenceToolRead<'a> = Box<EvidenceToolReadFn<'a>>;

#[cfg(feature = "test-support")]
fn capture_evidence_error(
    hook: Option<&EvidenceErrorHook>,
    id: &serde_json::Value,
    stage: &'static str,
    error: &anyhow::Error,
) {
    struct LimitedText<'a>(&'a mut String);
    impl std::fmt::Write for LimitedText<'_> {
        fn write_str(&mut self, part: &str) -> std::fmt::Result {
            let available = 1024 - self.0.len();
            if part.len() <= available {
                self.0.push_str(part);
                return Ok(());
            }
            let boundary = (0..=available)
                .rev()
                .find(|&n| part.is_char_boundary(n))
                .unwrap();
            self.0.push_str(&part[..boundary]);
            Err(std::fmt::Error)
        }
    }
    if let Some(hook) = hook {
        let mut detail = String::new();
        for cause in error.chain().take(8) {
            if !detail.is_empty() {
                if detail.len() + 2 > 1024 {
                    break;
                }
                detail.push_str(": ");
            }
            if std::fmt::write(&mut LimitedText(&mut detail), format_args!("{cause}")).is_err() {
                break;
            }
        }
        hook(id, stage, &detail);
    }
}

/// The shared cached-evidence read for find, inspect and read_source. A cold
/// checkout must show a prior head before it is activated; then one snapshot is
/// admitted, read, and finished. Store contention, including the cold check's,
/// retries until the call deadline.
#[allow(clippy::too_many_arguments)]
fn evidence_read(
    registry: &std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    key: &str,
    daemon_handle: Option<&tokio::runtime::Handle>,
    selected: Option<&crate::daemon::registry::SelectedCheckout>,
    deadline: std::time::Instant,
    clock_now: &dyn Fn() -> std::time::Instant,
    cancel: &std::sync::Arc<std::sync::atomic::AtomicBool>,
    publish: &dyn Fn(bool),
    id: &serde_json::Value,
    #[cfg(feature = "test-support")] evidence_error: Option<&EvidenceErrorHook>,
    query: Option<&str>,
    read: &EvidenceToolReadFn<'_>,
) -> anyhow::Result<(serde_json::Value, bool)> {
    let _profile = query.map(|_| crate::search_model::SearchPhaseTimer::start("search_total"));
    let cancelled = || cancel.load(std::sync::atomic::Ordering::Acquire);
    let runtime = loop {
        if let Some(runtime) = registry.blocking_lock().runtime(key) {
            break runtime;
        }
        // A cold evidence call must not activate a checkout merely to discover
        // that it has no derived index yet. Check outside the registry lock.
        let probe = registry.blocking_lock().prior_head_probe(key)?;
        match probe.check() {
            Ok(()) => {}
            Err(error) if crate::store::cli_read_retryable_contention(&error) => {
                if cancelled() || clock_now() >= deadline {
                    anyhow::bail!("deadline_exceeded: MCP selected read deadline");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            }
            Err(error) => return Err(error),
        }
        break match daemon_handle {
            Some(handle) => handle.block_on(async { registry.lock().await.activate(key) })?,
            None => anyhow::bail!("index_not_ready: checkout not activated"),
        };
    };
    #[cfg(feature = "test-support")]
    runtime.run_mcp_read_hook_for_tests();
    let search_head = query
        .and_then(|_| runtime.browser_scheduler().ok())
        .and_then(|scheduler| scheduler.search_head_slot(true));
    let search_query = query
        .map(|query| {
            if cancelled() || runtime.mcp_clock_now() >= deadline {
                anyhow::bail!("deadline_exceeded: MCP search query deadline");
            }
            let budget_clock = runtime.clone();
            let budget = crate::search_embed::QueryBudget::new(
                deadline
                    .checked_sub(SEARCH_LEXICAL_RESERVE)
                    .unwrap_or(deadline),
                std::sync::Arc::new(move || budget_clock.mcp_clock_now()),
            );
            match runtime.search_service() {
                Some(search) => {
                    if search.model_configured()
                        && let Some(head) = &search_head
                        && !head.resident()?
                    {
                        return Ok(search.unavailable_query());
                    }
                    match search.query_embedding(query, cancel, deadline, Some(budget.clone())) {
                        Ok(prepared) => Ok(prepared),
                        Err(crate::search_embed::EmbedError::BudgetExceeded) => {
                            Ok(crate::daemon::search::SearchQuery::budget_exceeded(
                                Some(search.status_handle().backend()),
                                budget,
                            ))
                        }
                        Err(crate::search_embed::EmbedError::Cancelled) => Err(anyhow::anyhow!(
                            "deadline_exceeded: MCP search query cancelled"
                        )),
                        Err(error) => Err(anyhow::Error::new(error)),
                    }
                }
                None => Ok(crate::daemon::search::SearchQuery::unavailable(None)),
            }
        })
        .transpose()?;
    loop {
        if cancelled() || runtime.mcp_clock_now() >= deadline {
            anyhow::bail!("deadline_exceeded: MCP selected read deadline");
        }
        let attempt = (|| {
            let (response, sampled) = match selected {
                Some(witness) => runtime.evidence_for_selected(witness)?,
                None => runtime.evidence_response()?,
            };
            publish(sampled);
            #[cfg(feature = "test-support")]
            runtime.run_mcp_admitted_gate_for_tests();
            if cancelled() || runtime.mcp_clock_now() >= deadline {
                anyhow::bail!("deadline_exceeded: MCP selected read deadline");
            }
            #[cfg(feature = "test-support")]
            let status = response.status().inspect_err(|error| {
                capture_evidence_error(evidence_error, id, "status", error);
            })?;
            #[cfg(not(feature = "test-support"))]
            let status = response.status()?;
            let pin = status.revision;
            let mut envelope = match read(&runtime, &response, sampled, search_query.as_ref())
                .map_err(|error| response.report_index_failure(error))
            {
                Ok(envelope) => envelope,
                Err(error) if crate::store::cli_read_retryable_contention(&error) => {
                    return Err(error);
                }
                Err(error) => {
                    #[cfg(feature = "test-support")]
                    capture_evidence_error(evidence_error, id, "read", &error);
                    tools::failure(id, evidence_failure_code(&error))
                }
            };
            // W8: a rejected store reports no basis; every other failure reports
            // the admitted snapshot's.
            if envelope.get("error").is_some() && envelope["error"]["code"] != "store_unavailable" {
                envelope["error"]["currentBasis"] = serde_json::json!({
                    "indexGeneration": pin.index_generation.to_string(),
                    "indexRevision": pin.index_revision
                });
            }
            #[cfg(feature = "test-support")]
            let envelope = response.finish(envelope).inspect_err(|error| {
                capture_evidence_error(evidence_error, id, "finish", error);
            })?;
            #[cfg(not(feature = "test-support"))]
            let envelope = response.finish(envelope)?;
            Ok((envelope, sampled))
        })();
        match attempt {
            Err(ref error) if crate::store::cli_read_retryable_contention(error) => {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            answer => return answer,
        }
    }
}

struct DescribedSnapshot {
    data: Option<(crate::model::IndexStatus, [[u64; 5]; 4])>,
    catching_up: bool,
    state: &'static str,
    progress: Option<serde_json::Value>,
}

/// Every tool call is answered within this deadline; describe reports it as
/// `limits.deadlineMs` (contract W1).
pub const CALL_DEADLINE: std::time::Duration = std::time::Duration::from_millis(5000);

/// Keep mandatory lexical/projection work inside the original call deadline.
const SEARCH_LEXICAL_RESERVE: std::time::Duration = std::time::Duration::from_millis(2000);

/// Retry only direct Store contention. Every failed attempt drops its selected
/// response; the one successful answer is materialized and finished in one snapshot.
fn read_describe(
    runtime: &crate::daemon::registry::CheckoutRuntime,
    selected: Option<&crate::daemon::registry::SelectedCheckout>,
    deadline: std::time::Instant,
    cancel: &std::sync::atomic::AtomicBool,
    admitted: &dyn Fn(bool),
) -> anyhow::Result<DescribedSnapshot> {
    #[cfg(feature = "test-support")]
    runtime.run_mcp_read_hook_for_tests();
    loop {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("deadline_exceeded: MCP call reached its deadline");
        }
        let attempt =
            runtime
                .mcp_describe(selected)
                .and_then(|(response, catching_up, state, progress)| {
                    // Read admission sampled catchingUp: from here a deadline
                    // answer uses the resolved shape (Decision 0009 §7).
                    admitted(catching_up);
                    #[cfg(feature = "test-support")]
                    runtime.run_mcp_admitted_gate_for_tests();
                    let data = response
                        .map(|response| {
                            let data = (|| {
                                let status = response.status()?;
                                let counts = response.mcp_language_counts()?;
                                Ok((status, counts))
                            })()
                            .map_err(|error| response.report_index_failure(error))?;
                            response.finish(data)
                        })
                        .transpose()?;
                    Ok(DescribedSnapshot {
                        data,
                        catching_up,
                        state,
                        progress,
                    })
                });
        match attempt {
            Err(error) if crate::store::cli_read_retryable_contention(&error) => {
                if runtime.mcp_clock_now() >= deadline {
                    anyhow::bail!("deadline_exceeded: MCP selected read was busy");
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            answer => return answer,
        }
    }
}

fn catching_up(
    registry: &Option<(
        std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
        u64,
    )>,
    key: &str,
) -> bool {
    registry
        .as_ref()
        .and_then(|(registry, _)| registry.blocking_lock().runtime(key))
        .is_none_or(|runtime| runtime.catching_up())
}

/// How soon a final selected check that could not finish is started again
/// (#131), while the coordinator keeps deciding the connection's other calls.
const FINAL_RECHECK_TICK: std::time::Duration = std::time::Duration::from_millis(10);

/// An opt-in, bounded fixture barrier at an actual coordinator boundary.
/// Normal MCP calls do not connect to or wait for a test socket.
#[cfg(feature = "test-support")]
pub(crate) fn phase_hook(stage: &str) -> std::io::Result<()> {
    use std::io::{Read, Write};
    // One stage, or several separated by commas.
    if !std::env::var("TRELLIS_TEST_MCP_PHASE")
        .is_ok_and(|stages| stages.split(',').any(|named| named == stage))
    {
        return Ok(());
    }
    let path = std::env::var_os("TRELLIS_TEST_MCP_PHASE_SOCKET").ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing MCP fixture socket",
        )
    })?;
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    stream.write_all(stage.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut release = [0u8; 1];
    stream.read_exact(&mut release)?;
    Ok(())
}

#[cfg(not(feature = "test-support"))]
pub(crate) fn phase_hook(_: &str) -> std::io::Result<()> {
    Ok(())
}

/// Call workers alive across all connections, each holding a permit until
/// its thread exits, even after its call was decided (Decision 0009 §7).
static CALL_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_CALL_WORKERS: usize = 16;
/// Outstanding calls per connection: a fifth is refused without queueing.
const MAX_CONNECTION_CALLS: usize = 4;

/// Call workers alive in this process, for tests of the worker budget.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub fn live_call_workers_for_tests() -> usize {
    CALL_WORKERS.load(std::sync::atomic::Ordering::Acquire)
}

struct WorkerPermit;
impl WorkerPermit {
    fn acquire() -> Option<Self> {
        use std::sync::atomic::Ordering;
        let mut live = CALL_WORKERS.load(Ordering::Acquire);
        while live < MAX_CALL_WORKERS {
            match CALL_WORKERS.compare_exchange_weak(
                live,
                live + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(WorkerPermit),
                Err(current) => live = current,
            }
        }
        None
    }
}
impl Drop for WorkerPermit {
    fn drop(&mut self) {
        CALL_WORKERS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

type Registry = Option<(
    std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    u64,
)>;

enum Input {
    /// A frame, its relay ID on the relayed transport, the instant the reader
    /// completed it (protocol acceptance), and its position in the input.
    Frame(wire::Frame, Option<u64>, std::time::Instant, u64),
    Done(u64, std::io::Result<CallOutcome>),
    /// A call's final selected-root check, run off the coordinator (#131).
    FinalChecked(u64, CheckedResult),
}

type CheckedResult = std::io::Result<
    Result<crate::daemon::registry::CommonDirs, crate::daemon::registry::SelectionError>,
>;

/// A selected call's send-time identity gate (#131): the common directories
/// its helper discovered, compared again on the coordinator just before the
/// call's response is committed.
struct SendGate {
    witness: crate::daemon::registry::SelectedCheckout,
    dirs: crate::daemon::registry::CommonDirs,
    catching_up: bool,
    capacity: bool,
}

/// A finished selected call whose final identity check is running, or due to
/// start again (#131). At most one check runs per call.
struct FinalCheck {
    outcome: CallOutcome,
    running: bool,
    retry_at: Option<std::time::Instant>,
}

/// Start the slow part of `token`'s final selected-root check, discovering
/// both common Git directories, on its own thread; its result reaches the
/// coordinator's inbox. The thread holds no registry or session
/// lock, and a result for a call decided meanwhile is ignored.
fn start_final_check(
    token: u64,
    witness: &crate::daemon::registry::SelectedCheckout,
    deadline: std::time::Instant,
    stage: &'static str,
    inbox: &std::sync::mpsc::Sender<Input>,
) -> bool {
    let (witness, inbox) = (witness.clone(), inbox.clone());
    std::thread::Builder::new()
        .name("mcp-final-check".into())
        .spawn(move || {
            let checked = phase_hook(stage).map(|()| witness.common_dirs_by(Some(deadline)));
            // The coordinator may have ended: nobody waits for this result.
            let _ = inbox.send(Input::FinalChecked(token, checked));
        })
        .is_ok()
}

/// A worker's finished call: the tool result and the fences the send decision
/// rechecks.
struct CallOutcome {
    result: serde_json::Value,
    finals: Finals,
}
enum Finals {
    /// Invalid application fields or a selection failure: nothing to recheck.
    None,
    Launch {
        catching_up: bool,
    },
    Selected {
        witness: crate::daemon::registry::SelectedCheckout,
        catching_up: bool,
    },
    Capacity {
        witness: crate::daemon::registry::SelectedCheckout,
        catching_up: bool,
    },
}

/// The answering root and `catchingUp`, once read admission sampled it.
type Resolved = std::sync::Arc<std::sync::Mutex<Option<(std::path::PathBuf, bool)>>>;

struct InFlight {
    id: serde_json::Value,
    name: String,
    relay: Option<u64>,
    modern: bool,
    deadline: std::time::Instant,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    resolved: Resolved,
    attempted: serde_json::Value,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.cancel
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

struct CallContext {
    workspace: std::sync::Arc<OpenedWorkspace>,
    registry: Registry,
    daemon_handle: Option<tokio::runtime::Handle>,
    id: serde_json::Value,
    modern: bool,
    name: String,
    arguments: Option<serde_json::Value>,
    deadline: std::time::Instant,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    resolved: Resolved,
}

fn commit_selection(
    registry: &std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    session: u64,
    prepared: crate::daemon::registry::PreparedSelection,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<crate::daemon::registry::SelectedCheckout, crate::daemon::registry::SelectionError> {
    let mut registry = registry.blocking_lock();
    // EOF cancels before disconnect takes this same guard. Either admission
    // wins first and disconnect removes it, or a late admission is refused.
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return Err(crate::daemon::registry::SelectionError::Unavailable);
    }
    registry.select_prepared(session, prepared)
}

#[cfg(test)]
mod selection_commit_tests {
    use super::*;

    #[test]
    fn disconnect_cancels_a_prepared_selection_before_owner_commit() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(&root)
            .status()
            .unwrap();
        assert!(status.success());
        let root = root.canonicalize().unwrap();
        let identity =
            crate::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let registry = std::sync::Arc::new(tokio::sync::Mutex::new(
            crate::daemon::registry::CheckoutRegistry::new(),
        ));
        registry
            .blocking_lock()
            .attach_launch(7, &identity)
            .unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (prepared_tx, prepared_rx) = std::sync::mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
        let worker_registry = registry.clone();
        let worker_cancel = cancel.clone();
        let worker = std::thread::spawn(move || {
            let probe = worker_registry.blocking_lock().selection_probe();
            let prepared = probe.prepare(&identity, &root).unwrap();
            prepared_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            commit_selection(&worker_registry, 7, prepared, &worker_cancel)
        });
        prepared_rx
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap();
        cancel.store(true, std::sync::atomic::Ordering::Release);
        registry.blocking_lock().disconnect(7);
        resume_tx.send(()).unwrap();
        assert!(matches!(
            worker.join().unwrap(),
            Err(crate::daemon::registry::SelectionError::Unavailable)
        ));
        let guard = registry.blocking_lock();
        assert!(guard.idle_exit_deadline().is_some());
        let tick_at = std::time::Instant::now() + crate::daemon::registry::DAEMON_IDLE_DELAY;
        drop(guard);
        let tick = registry.blocking_lock().advance(tick_at).unwrap();
        assert!(
            tick.exit,
            "a late selection must not resurrect the disconnected session"
        );
    }
}

/// One admitted call, run whole on its worker: selection, readiness, the
/// read and the response. The connection decides whether to send it.
fn run_call(ctx: CallContext) -> std::io::Result<CallOutcome> {
    let CallContext {
        workspace,
        registry,
        daemon_handle,
        id,
        modern,
        name,
        arguments,
        deadline,
        cancel,
        resolved,
    } = ctx;
    phase_hook("call_worker")?;
    let selection = arguments
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("workspace"));
    let selected = if let Some((registry, session_id)) = &registry {
        match selection.and_then(serde_json::Value::as_str) {
            Some(path) => {
                let probe = registry.blocking_lock().selection_probe();
                probe
                    .prepare(workspace.identity(), std::path::Path::new(path))
                    .and_then(|prepared| commit_selection(registry, *session_id, prepared, &cancel))
                    .map(Some)
            }
            None => workspace
                .check()
                .map(|_| None)
                .map_err(|_| crate::daemon::registry::SelectionError::IdentityChanged),
        }
    } else {
        workspace
            .check()
            .map(|_| None)
            .map_err(|_| crate::daemon::registry::SelectionError::IdentityChanged)
    };
    match selected {
        Ok(selected) => {
            if selected.is_some() {
                phase_hook("selected_clone")?;
            }
            let key = selected
                .as_ref()
                .map_or(&workspace.identity.root_key, |w| &w.identity.root_key);
            let catching_up = catching_up(&registry, key);
            let selected_workspace = selected
                .as_ref()
                .map(|w| w.identity.verified_clone().map(OpenedWorkspace::new));
            let selected_workspace = match selected_workspace.transpose() {
                Err(_) => {
                    let root = selected
                        .as_ref()
                        .map_or(workspace.root(), |witness| witness.identity.root.as_path());
                    return Ok(CallOutcome {
                        result: tools::resolved_root_changed(&name, &id, root, catching_up, modern),
                        finals: Finals::None,
                    });
                }
                Ok(selected_workspace) => selected_workspace,
            };
            let answering = selected_workspace.as_ref().unwrap_or(&workspace);
            let mut tool = tools::prepare(&name, arguments.as_ref(), &id, modern, answering);
            let mut sampled_catching_up = catching_up;
            let mut envelope = tool.response["structuredContent"].clone();
            if name == "trellis_workspace_describe"
                && let Some((registry, _)) = &registry
            {
                let answering_root = answering.root().to_owned();
                let publish = |catching_up: bool| {
                    *resolved.lock().unwrap() = Some((answering_root.clone(), catching_up));
                };
                match describe(
                    registry,
                    key,
                    selected.as_ref(),
                    daemon_handle.as_ref(),
                    deadline,
                    &cancel,
                    &publish,
                ) {
                    Ok(DescribeResult {
                        read,
                        degraded,
                        search_availability,
                    }) => {
                        match read {
                            Ok(DescribedSnapshot {
                                data: Some((status, counts)),
                                catching_up: sampled,
                                state,
                                progress,
                            }) => {
                                sampled_catching_up = sampled;
                                envelope = projection::describe(
                                    &id,
                                    answering.label(),
                                    state,
                                    progress,
                                    Some(counts),
                                    Some(serde_json::json!({
                                        "indexGeneration": status.revision.index_generation.to_string(),
                                        "indexRevision": status.revision.index_revision
                                    })),
                                    false,
                                );
                            }
                            Ok(DescribedSnapshot {
                                data: None,
                                catching_up: sampled,
                                state,
                                progress,
                            }) => {
                                sampled_catching_up = sampled;
                                envelope = projection::describe(
                                    &id,
                                    answering.label(),
                                    state,
                                    progress,
                                    None,
                                    None,
                                    false,
                                );
                            }
                            Err(error) => {
                                envelope = tools::failure(&id, mcp_read_error(&error));
                            }
                        }
                        match degraded {
                            Ok(degraded) if envelope.get("data").is_some() => {
                                envelope["data"]["watcherDegraded"] = serde_json::json!(degraded);
                                envelope["data"]["searchIndex"] =
                                    serde_json::to_value(search_availability)?;
                            }
                            Err(error) => envelope = tools::failure(&id, mcp_read_error(&error)),
                            _ => {}
                        }
                    }
                    Err(error) => {
                        envelope = tools::failure(&id, mcp_read_error(&error));
                    }
                }
            }
            let args = arguments.as_ref().and_then(serde_json::Value::as_object);
            let root = answering.root();
            let id = &id;
            // The coordinator stamped `deadline = accepted + call_deadline`.
            let accepted = deadline
                .checked_sub(workspace.call_deadline)
                .unwrap_or(deadline);
            let cancel = &cancel;
            let tool_read: Option<EvidenceToolRead> = match (name.as_str(), args) {
                _ if !tool.valid_arguments || envelope["error"]["code"] != "index_not_ready" => {
                    None
                }
                (name, Some(args))
                    if architecture::is_tool(name) || name == architecture::PROMPT_NAME =>
                {
                    let kind = architecture::operation(name);
                    let arguments = serde_json::Value::Object(args.clone());
                    Some(Box::new(move |runtime, response, _sampled, _query| {
                        let mut check = || {
                            if cancel.load(std::sync::atomic::Ordering::Acquire)
                                || runtime.mcp_clock_now() >= deadline
                            {
                                anyhow::bail!("deadline_exceeded: architecture projection");
                            }
                            Ok(())
                        };
                        match response.architecture_response(kind, &arguments, &mut check) {
                            Ok(payload) => Ok(architecture::envelope(id, payload)),
                            Err(error) if crate::store::cli_read_retryable_contention(&error) => {
                                Err(error)
                            }
                            Err(error) => Ok(architecture::failure(id, &error)),
                        }
                    }))
                }
                ("trellis_search_symbols", Some(args)) => {
                    let query = args["query"].as_str().expect("validated search query");
                    let limit = args.get("limit").map_or(search::DEFAULT_LIMIT, |value| {
                        wire::uint(value).expect("validated search limit") as usize
                    });
                    let role =
                        args.get("role")
                            .map_or(crate::search_rank::SearchRole::Any, |value| {
                                serde_json::from_value(value.clone())
                                    .expect("validated search role")
                            });
                    let expected = validated_expected_basis(args);
                    Some(Box::new(move |runtime, response, sampled, prepared| {
                        let prepared = prepared.expect("search query prepared before admission");
                        let mut check = || {
                            if cancel.load(std::sync::atomic::Ordering::Acquire)
                                || runtime.mcp_clock_now() >= deadline
                            {
                                anyhow::bail!("deadline_exceeded: MCP search deadline");
                            }
                            Ok(())
                        };
                        let pin = response.status()?.revision;
                        anyhow::ensure!(
                            expected.is_none_or(|expected| expected == pin),
                            "revision conflict: expected basis differs from admitted head"
                        );
                        let policy = response.mcp_search_policy(query, pin, role, &mut check)?;
                        let service = runtime.search_service();
                        let head = runtime.browser_scheduler()?.search_head_slot(false);
                        if service.is_some_and(|service| service.model_configured())
                            && let Some(head) = &head
                        {
                            head.fault()?;
                        }
                        let warm = match (&head, service, prepared.model_id()) {
                            (Some(head), Some(service), Some(model)) => {
                                head.declarations(pin, model, service.cache_epoch())?
                            }
                            _ => None,
                        };
                        let resident = if let Some(declarations) = warm {
                            let lexical = response.mcp_search_lexical(
                                query,
                                pin,
                                declarations,
                                &policy,
                                &mut check,
                            )?;
                            let head = head.as_ref().expect("published head selected");
                            let service = service.expect("published head has service");
                            let mut vector_check = || {
                                check()?;
                                if let Some(budget) = prepared.budget() {
                                    budget.check()?;
                                }
                                Ok(())
                            };
                            let ranked = head.rank(
                                pin,
                                prepared.model_id().expect("published head has model"),
                                service.cache_epoch(),
                                &lexical,
                                prepared.vector.as_deref(),
                                &policy,
                                &mut vector_check,
                            );
                            match ranked {
                                Ok(Some(ranked)) => Some((declarations, Ok(ranked), lexical)),
                                Ok(None) => None,
                                Err(error) => Some((declarations, Err(error), lexical)),
                            }
                        } else {
                            None
                        };
                        let resident_used = resident.is_some();
                        let (declarations, ranked, lexical) = match resident {
                            Some(resident) => resident,
                            None if head.is_some() => {
                                // Cold, capped, or concurrently replaced matrices remain
                                // lexical-only at this admitted pin. Never stream vectors
                                // around the resident-memory policy or wait for the owner.
                                let declarations =
                                    response.mcp_search_head_count(pin, &mut check)?;
                                let lexical = response.mcp_search_lexical(
                                    query,
                                    pin,
                                    declarations,
                                    &policy,
                                    &mut check,
                                )?;
                                let ranked =
                                    crate::search_rank::fuse_lexical(&lexical, &policy, &mut check);
                                (declarations, ranked, lexical)
                            }
                            None => {
                                // Standalone registries have no matrix owner.
                                let candidates = response.mcp_search_candidates(
                                    query,
                                    Some(pin),
                                    &policy,
                                    &mut check,
                                )?;
                                let mut vector_check = || {
                                    check()?;
                                    if let Some(budget) = prepared.budget() {
                                        budget.check()?;
                                    }
                                    Ok(())
                                };
                                let rank = |snapshot: Option<
                                    &crate::search_vectors::VectorSnapshot<'_>,
                                >| {
                                    crate::search_rank::rank_snapshot(
                                        &candidates.cards,
                                        &candidates.lexical,
                                        snapshot,
                                        prepared.model_id(),
                                        prepared.vector.as_deref(),
                                        &policy,
                                        &mut vector_check,
                                    )
                                };
                                let ranked = match service {
                                    Some(service) if prepared.model_id().is_some() => {
                                        service.with_vector_snapshot_budget(prepared.budget(), rank)
                                    }
                                    _ => crate::search_rank::rank_lexical(
                                        &candidates.cards,
                                        &candidates.lexical,
                                        &policy,
                                        &mut check,
                                    ),
                                };
                                (candidates.cards.len(), ranked, candidates.lexical)
                            }
                        };
                        let (ranked, budget_exceeded) = match ranked {
                            Ok(ranked) => (ranked, false),
                            Err(error)
                                if matches!(
                                    error.downcast_ref::<crate::search_embed::EmbedError>(),
                                    Some(crate::search_embed::EmbedError::BudgetExceeded)
                                ) =>
                            {
                                (
                                    crate::search_rank::fuse_lexical(
                                        &lexical, &policy, &mut check,
                                    )?,
                                    true,
                                )
                            }
                            Err(error) if crate::store::cli_read_retryable_contention(&error) => {
                                if prepared
                                    .budget()
                                    .is_some_and(|budget| budget.check().is_err())
                                {
                                    check()?;
                                    (
                                        crate::search_rank::fuse_lexical(
                                            &lexical, &policy, &mut check,
                                        )?,
                                        true,
                                    )
                                } else {
                                    return Err(error);
                                }
                            }
                            Err(error) if error.is::<crate::search_vectors::StoreUnavailable>() => {
                                return Ok(tools::failure(id, "store_unavailable"));
                            }
                            Err(error) => return Err(error),
                        };
                        let building = runtime.browser_scheduler()?.search_fill_building(pin);
                        let mut search_index =
                            prepared.status(declarations as u64, ranked.embedded, building);
                        if head.is_some() && !resident_used {
                            search_index.state = crate::search_model::SearchIndexState::Unavailable;
                            search_index.model_id = None;
                            search_index.model_name = None;
                            search_index.embedded = 0;
                        }
                        if budget_exceeded {
                            search_index.state = crate::search_model::SearchIndexState::Unavailable;
                            search_index.model_id = None;
                            search_index.model_name = None;
                            search_index.embedded = 0;
                            search_index.unavailable_reason = Some(
                                crate::search_model::SearchUnavailableReason::VectorBudgetExceeded,
                            );
                        }
                        let has_more = ranked.hits.len() > limit;
                        let prefix = &ranked.hits[..ranked.hits.len().min(limit)];
                        let keys: Vec<_> = prefix.iter().map(|hit| hit.key.clone()).collect();
                        let matched_by = prefix
                            .iter()
                            .map(|hit| {
                                let mut arms = Vec::new();
                                if hit.lexical_rank.is_some() {
                                    arms.push("lexical".to_owned());
                                }
                                if hit.vector_rank.is_some() {
                                    arms.push("vector".to_owned());
                                }
                                arms
                            })
                            .collect();
                        let mut symbols = response.mcp_search_hydrate(&keys, pin, &mut check)?;
                        symbols.has_more = has_more;
                        projection::search(
                            id,
                            search::SearchResult {
                                symbols,
                                matched_by,
                                search_index,
                                role,
                            },
                            root,
                            sampled,
                            modern,
                        )
                        .map_err(|error| anyhow::anyhow!(error))
                        .map(|envelope| {
                            envelope.unwrap_or_else(|| tools::failure(id, "range_too_large"))
                        })
                    }))
                }
                ("trellis_find_symbols", Some(args)) => {
                    let query = args["query"].as_str().expect("validated find query");
                    let limit = args
                        .get("limit")
                        .map_or(20, |v| wire::uint(v).expect("validated limit") as usize);
                    let expected = validated_expected_basis(args);
                    Some(Box::new(move |_, response, sampled, _query| {
                        let found = response.mcp_find_with_lines(query, limit, expected)?;
                        Ok(projection::find(id, found, root, sampled, modern)
                            .map_err(|error| anyhow::anyhow!(error))?
                            .unwrap_or_else(|| tools::failure(id, "range_too_large")))
                    }))
                }
                ("trellis_declaration", Some(args)) => {
                    let symbol_id = args["symbolId"].as_str().expect("validated declaration ID");
                    let expected = validated_expected_basis(args);
                    Some(Box::new(move |_, response, sampled, _query| {
                        Ok(
                            match response.mcp_declaration_with_lines(symbol_id, expected)? {
                                Some(found) => {
                                    projection::declaration(id, found, root, sampled, modern)
                                        .unwrap_or_else(|| tools::failure(id, "range_too_large"))
                                }
                                None => tools::failure(id, "not_found"),
                            },
                        )
                    }))
                }
                ("trellis_outgoing_calls", Some(args)) => {
                    let symbol_id = args["symbolId"].as_str().expect("validated outgoing ID");
                    let limit = args
                        .get("limit")
                        .map_or(20, |v| wire::uint(v).expect("validated limit") as usize);
                    let expected = validated_expected_basis(args);
                    Some(Box::new(move |runtime, response, sampled, _query| {
                        Ok(
                            match response.mcp_outgoing_with_lines(
                                symbol_id,
                                limit,
                                expected,
                                &|| runtime.mcp_hint_clock_now(accepted),
                                deadline,
                            )? {
                                Some(found) => {
                                    projection::outgoing(id, found, root, sampled, modern)
                                        .map_err(|error| anyhow::anyhow!(error))?
                                        .unwrap_or_else(|| tools::failure(id, "range_too_large"))
                                }
                                None => tools::failure(id, "not_found"),
                            },
                        )
                    }))
                }
                ("trellis_incoming_calls", Some(args)) => {
                    let symbol_id = args["symbolId"].as_str().expect("validated incoming ID");
                    let limit = args
                        .get("limit")
                        .map_or(20, |v| wire::uint(v).expect("validated limit") as usize);
                    let expected = validated_expected_basis(args);
                    Some(Box::new(move |_, response, sampled, _query| {
                        Ok(
                            match response.mcp_incoming_with_lines(symbol_id, limit, expected)? {
                                Some(found) => {
                                    projection::incoming(id, found, root, sampled, modern)
                                        .map_err(|error| anyhow::anyhow!(error))?
                                        .unwrap_or_else(|| tools::failure(id, "range_too_large"))
                                }
                                None => tools::failure(id, "not_found"),
                            },
                        )
                    }))
                }
                ("trellis_call_graph", Some(args)) => {
                    let query = tools::call_graph_query(&serde_json::Value::Object(args.clone()));
                    let expected = validated_expected_basis(args);
                    Some(Box::new(move |runtime, response, sampled, _query| {
                        let mut check = || {
                            if cancel.load(std::sync::atomic::Ordering::Acquire)
                                || runtime.mcp_clock_now() >= deadline
                            {
                                anyhow::bail!("deadline_exceeded: MCP call graph deadline");
                            }
                            Ok(())
                        };
                        let mut answer = response.graph_query(&query, expected, &mut check)?;
                        if let Err(error) = &answer.outcome {
                            return Ok(call_graph_failure(id, error));
                        }
                        Ok(
                            projection::call_graph(id, &mut answer, root, sampled, modern)
                                .map_err(|error| anyhow::anyhow!(error))?
                                .unwrap_or_else(|| tools::failure(id, "range_too_large")),
                        )
                    }))
                }
                ("trellis_read_source", Some(args)) => {
                    let path = args["path"].as_str().expect("validated source path");
                    let start = wire::uint(&args["startLine"]).expect("validated source start");
                    let end = wire::uint(&args["endLine"]).expect("validated source end");
                    let expected_hash = args.get("expectedContentHash").and_then(|v| v.as_str());
                    let expected = validated_expected_basis(args);
                    Some(Box::new(
                        move |_, response, sampled, _query| match response.mcp_source(
                            path,
                            start,
                            end,
                            expected,
                            expected_hash,
                        ) {
                            Ok(Some(found)) => {
                                Ok(projection::source(id, found, root, sampled, modern)
                                    .unwrap_or_else(|| tools::failure(id, "range_too_large")))
                            }
                            Ok(None) => Ok(tools::failure(id, "not_found")),
                            Err(error)
                                if !crate::store::cli_read_retryable_contention(&error)
                                    && error.to_string().starts_with("revision conflict:")
                                    && error.to_string().contains("content hash") =>
                            {
                                let mut envelope = tools::failure(id, "revision_conflict");
                                if let Some(hash) = response.mcp_source_hash(path)? {
                                    envelope["error"]["currentContentHash"] =
                                        serde_json::json!(hash);
                                }
                                Ok(envelope)
                            }
                            Err(error) => Err(error),
                        },
                    ))
                }
                _ => None,
            };
            if let Some(tool_read) = tool_read
                && let Some((registry, _)) = &registry
            {
                let publish = |sampled: bool| {
                    *resolved.lock().unwrap() = Some((root.to_owned(), sampled));
                };
                match evidence_read(
                    registry,
                    key,
                    daemon_handle.as_ref(),
                    selected.as_ref(),
                    deadline,
                    &|| workspace.clock_now(),
                    cancel,
                    &publish,
                    id,
                    #[cfg(feature = "test-support")]
                    workspace.hooks.evidence_error.as_ref(),
                    (name == "trellis_search_symbols").then(|| {
                        args.expect("validated search arguments")["query"]
                            .as_str()
                            .expect("validated search query")
                    }),
                    &*tool_read,
                ) {
                    Ok((found, sampled)) => {
                        envelope = found;
                        sampled_catching_up = sampled;
                    }
                    Err(error) => {
                        let code = evidence_failure_code(&error);
                        if code == "deadline_exceeded" && resolved.lock().unwrap().is_none() {
                            // No read was admitted, so the call never resolved:
                            // answer in the pre-resolution shape (Decision 0009).
                            return Ok(CallOutcome {
                                result: tools::result(
                                    &name,
                                    tools::admission_failure(
                                        id,
                                        "deadline_exceeded",
                                        &attempted_workspace(arguments.as_ref(), &workspace),
                                    ),
                                    modern,
                                ),
                                finals: Finals::None,
                            });
                        }
                        envelope = tools::failure(id, code);
                    }
                }
            }
            *resolved.lock().unwrap() = Some((answering.root().to_owned(), sampled_catching_up));
            envelope["workspace"] = serde_json::json!(answering.root());
            envelope["catchingUp"] = serde_json::json!(sampled_catching_up);
            tool.response = if architecture::is_tool(&name) || name == architecture::PROMPT_NAME {
                architecture::capped(id, &name, envelope, modern)
            } else {
                tools::result(&name, envelope, modern)
            };
            let finals = match selected {
                Some(witness) => Finals::Selected {
                    witness,
                    catching_up: sampled_catching_up,
                },
                None => Finals::Launch {
                    catching_up: sampled_catching_up,
                },
            };
            Ok(CallOutcome {
                result: tool.response,
                finals,
            })
        }
        Err(crate::daemon::registry::SelectionError::CheckoutCapacity) => {
            let Some(path) = selection.and_then(serde_json::Value::as_str) else {
                unreachable!("validated capacity requires explicit selection")
            };
            let checked_capacity =
                crate::daemon::registry::CheckoutRegistry::capacity_witness_unlocked(
                    workspace.identity(),
                    std::path::Path::new(path),
                );
            match checked_capacity {
                Ok(witness) => {
                    let catching_up = catching_up(&registry, &witness.identity.root_key);
                    *resolved.lock().unwrap() = Some((witness.identity.root.clone(), catching_up));
                    let result = tools::result(
                        &name,
                        tools::attributed_root(
                            tools::failure(&id, "checkout_capacity"),
                            &witness.identity.root,
                            catching_up,
                        ),
                        modern,
                    );
                    Ok(CallOutcome {
                        result,
                        finals: Finals::Capacity {
                            witness,
                            catching_up,
                        },
                    })
                }
                Err(error) => Ok(CallOutcome {
                    result: tools::result(
                        &name,
                        tools::selection_failure(&id, error, &serde_json::json!(path)),
                        modern,
                    ),
                    finals: Finals::None,
                }),
            }
        }
        Err(error) => {
            let attempted = selection
                .cloned()
                .unwrap_or_else(|| serde_json::json!(workspace.root()));
            Ok(CallOutcome {
                result: tools::result(
                    &name,
                    tools::selection_failure(&id, error, &attempted),
                    modern,
                ),
                finals: Finals::None,
            })
        }
    }
}

/// Activation, the selected read and published watcher health for one
/// describe call, on the call's own worker.
struct DescribeResult {
    read: anyhow::Result<DescribedSnapshot>,
    degraded: anyhow::Result<bool>,
    search_availability: crate::search_model::SearchAvailability,
}

fn describe(
    registry: &std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    key: &str,
    selected: Option<&crate::daemon::registry::SelectedCheckout>,
    daemon_handle: Option<&tokio::runtime::Handle>,
    deadline: std::time::Instant,
    cancel: &std::sync::atomic::AtomicBool,
    admitted: &dyn Fn(bool),
) -> anyhow::Result<DescribeResult> {
    let existing = registry.blocking_lock().runtime(key);
    let runtime = match existing {
        Some(runtime) => runtime,
        None => match daemon_handle {
            Some(handle) => handle.block_on(async { registry.lock().await.activate(key) })?,
            None => anyhow::bail!("index_not_ready: checkout not activated"),
        },
    };
    let read = read_describe(&runtime, selected, deadline, cancel, admitted);
    Ok(DescribeResult {
        read,
        degraded: runtime.mcp_watcher_degraded(),
        search_availability: runtime.search_availability(),
    })
}

/// Outstanding non-call requests per connection; more are refused at once
/// (Decision 0009 §7, as for the relay).
const MAX_CONNECTION_REQUESTS: usize = 16;

/// The coordinator's input: worker completions, and frames in input order,
/// each numbered by the reader when complete. A decision settles every frame
/// completed before it, waiting for one the reader counted but has not yet
/// published, and no frame completed after it.
/// At most this many frames published and not yet taken by the coordinator.
const INPUT_WINDOW: usize = 32;

/// The reader's frame window. Worker results share the coordinator's channel
/// but never wait on it, so a client that keeps writing cannot starve them.
#[derive(Default)]
struct FrameWindow {
    published: std::sync::Mutex<usize>,
    taken: std::sync::Condvar,
}
impl FrameWindow {
    fn acquire(&self) {
        let mut published = self.published.lock().unwrap();
        while *published >= INPUT_WINDOW {
            published = self.taken.wait(published).unwrap();
        }
        *published += 1;
    }
    fn release(&self) {
        *self.published.lock().unwrap() -= 1;
        self.taken.notify_one();
    }
}

struct Inbox {
    receiver: std::sync::mpsc::Receiver<Input>,
    window: std::sync::Arc<FrameWindow>,
    /// Frames the reader has completed.
    completed: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Frames received from the channel.
    received: u64,
    /// The first frame completed after the last decision.
    later: Option<Input>,
    ended: bool,
    #[cfg(feature = "test-support")]
    waiting_hook: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}
impl Inbox {
    /// The decision point: every frame completed so far.
    fn decision(&self) -> u64 {
        self.completed.load(std::sync::atomic::Ordering::Acquire)
    }
    /// Next input completed by `until`: without blocking once every such
    /// frame has arrived, else waiting for the reader to publish it.
    fn settled(&mut self, until: u64) -> Option<Input> {
        let next = match self.later.take() {
            Some(input) => input,
            None if self.received >= until => match self.receiver.try_recv() {
                Ok(input) => input,
                Err(std::sync::mpsc::TryRecvError::Empty) => return None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.ended = true;
                    return None;
                }
            },
            None => {
                #[cfg(feature = "test-support")]
                if let Some(hook) = &self.waiting_hook {
                    hook();
                }
                match self.receiver.recv() {
                    Ok(input) => input,
                    Err(_) => {
                        self.ended = true;
                        return None;
                    }
                }
            }
        };
        match next {
            Input::Frame(_, _, _, seq) if seq > until => {
                self.later = Some(next);
                None
            }
            Input::Frame(frame, relay, accepted, seq) => {
                self.received = seq;
                self.window.release();
                Some(Input::Frame(frame, relay, accepted, seq))
            }
            input => Some(input),
        }
    }
    /// Wait for input until `deadline` (forever without one).
    fn wait(&mut self, deadline: Option<std::time::Instant>) -> Option<Input> {
        let input = match self.later.take() {
            Some(input) => input,
            None => {
                let received = match deadline {
                    Some(deadline) => match self
                        .receiver
                        .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                    {
                        Ok(input) => Ok(input),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return None,
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(()),
                    },
                    None => self.receiver.recv().map_err(|_| ()),
                };
                match received {
                    Ok(input) => input,
                    Err(()) => {
                        self.ended = true;
                        return None;
                    }
                }
            }
        };
        if let Input::Frame(_, _, _, seq) = &input {
            self.received = *seq;
            self.window.release();
        }
        Some(input)
    }
}

/// The connection coordinator (Decision 0009 §7). It alone owns the output.
/// It admits calls as they arrive, runs each on a bounded worker, and decides
/// each outcome at the earliest of its result and its deadline, after settling
/// all input received by that instant (so a cancellation wins over an unsent
/// result).
fn run_transport(
    workspace: OpenedWorkspace,
    input: impl std::io::Read + Send + 'static,
    output: impl std::io::Write,
    registry: Registry,
    daemon_handle: Option<tokio::runtime::Handle>,
    relayed: bool,
) -> std::io::Result<()> {
    // A client that hangs up while replies are still being written ends the
    // session; that is not a failure.
    match coordinate(workspace, input, output, registry, daemon_handle, relayed) {
        Err(error) if client_hung_up(&error) => Ok(()),
        other => other,
    }
}

/// The write errors a closed client peer produces. macOS can report a write
/// to a socket whose peer has gone as `NotConnected` (ENOTCONN) rather than
/// `BrokenPipe`.
fn client_hung_up(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
    )
}

#[cfg(test)]
mod hang_up_tests {
    use super::client_hung_up;
    use std::io::{Error, ErrorKind};

    #[test]
    fn a_closed_peer_is_a_hang_up_in_each_form_the_os_reports_it() {
        for kind in [
            ErrorKind::BrokenPipe,
            ErrorKind::ConnectionReset,
            ErrorKind::NotConnected,
        ] {
            assert!(client_hung_up(&Error::from(kind)), "{kind:?}");
        }
        // ENOTCONN as macOS reports it on a write to a socket whose peer closed.
        assert!(client_hung_up(&Error::from_raw_os_error(libc::ENOTCONN)));
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidData,
            ErrorKind::TimedOut,
        ] {
            assert!(!client_hung_up(&Error::from(kind)), "{kind:?}");
        }
    }
}

fn coordinate(
    workspace: OpenedWorkspace,
    input: impl std::io::Read + Send + 'static,
    output: impl std::io::Write,
    registry: Registry,
    daemon_handle: Option<tokio::runtime::Handle>,
    relayed: bool,
) -> std::io::Result<()> {
    use std::collections::{HashMap, VecDeque};
    use std::io::BufReader;
    use std::time::Instant;
    let workspace = std::sync::Arc::new(workspace);
    let (sender, receiver) = std::sync::mpsc::channel::<Input>();
    let reader_sender = sender.clone();
    let window = std::sync::Arc::new(FrameWindow::default());
    let reader_window = window.clone();
    let reader_workspace = workspace.clone();
    #[cfg(feature = "test-support")]
    let frame_read = workspace.hooks.frame_read.clone();
    let completed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let counter = completed.clone();
    std::thread::Builder::new()
        .name("mcp-stdin".into())
        .spawn(move || {
            let mut reader = BufReader::new(input);
            loop {
                let read = if relayed {
                    wire::read_relayed_frame(&mut reader)
                } else {
                    wire::read_frame(&mut reader).map(|frame| (None, frame))
                };
                // Protocol acceptance: the deadline starts when the frame is
                // complete, not when the coordinator reaches it.
                let accepted = reader_workspace.clock_now();
                // Counted when complete, before it is published: a decision
                // that samples the count waits for every frame it covers.
                let seq = counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
                match read {
                    Ok((_, wire::Frame::Eof)) => {
                        reader_window.acquire();
                        let _ =
                            reader_sender.send(Input::Frame(wire::Frame::Eof, None, accepted, seq));
                        break;
                    }
                    Ok((relay, frame)) => {
                        #[cfg(feature = "test-support")]
                        if let Some(hook) = &frame_read {
                            hook();
                        }
                        reader_window.acquire();
                        if reader_sender
                            .send(Input::Frame(frame, relay, accepted, seq))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(error) => {
                        use std::io::Write;
                        let _ =
                            writeln!(std::io::stderr().lock(), "MCP stdin read failed: {error}");
                        reader_window.acquire();
                        let _ =
                            reader_sender.send(Input::Frame(wire::Frame::Eof, None, accepted, seq));
                        break;
                    }
                }
            }
        })?;
    let mut inbox = Inbox {
        receiver,
        window,
        completed,
        received: 0,
        later: None,
        ended: false,
        #[cfg(feature = "test-support")]
        waiting_hook: workspace.hooks.settle_waiting.clone(),
    };
    let mut session = session::Session::new();
    let mut completed: VecDeque<(u64, std::io::Result<CallOutcome>)> = VecDeque::new();
    let mut in_flight: HashMap<u64, InFlight> = HashMap::new();
    let mut writer = output;
    // One fence pass (Decision 0009 §7): handle every frame completed before
    // now, in order. A cancel removes its ID's pending admission; a request
    // joins `arrivals`, decided after a later fence; a worker result joins
    // `completed`. Nothing is written here. Returns true at end of input.
    let fence = |inbox: &mut Inbox,
                 session: &mut session::Session,
                 arrivals: &mut VecDeque<impl_receive::Outcome>,
                 completed: &mut VecDeque<(u64, std::io::Result<CallOutcome>)>,
                 checked: &mut VecDeque<(u64, CheckedResult)>|
     -> bool {
        let until = inbox.decision();
        while let Some(input) = inbox.settled(until) {
            match input {
                Input::Frame(frame, relay, accepted, _) => {
                    if receive(frame, relay, accepted, session, arrivals) {
                        return true;
                    }
                }
                Input::Done(token, outcome) => completed.push_back((token, outcome)),
                Input::FinalChecked(token, result) => checked.push_back((token, result)),
            }
        }
        if inbox.ended {
            session.eof();
            return true;
        }
        false
    };
    // A cancelled or ended call stops its worker cooperatively.
    let cancel_dropped = |session: &session::Session, in_flight: &mut HashMap<u64, InFlight>| {
        in_flight.retain(|token, call| {
            let pending = session.pending(*token).is_some();
            if !pending {
                call.cancel
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            pending
        });
    };
    let deadline_answer = |call: &InFlight| {
        let envelope = match &*call.resolved.lock().unwrap() {
            Some((root, catching_up)) => tools::attributed_root(
                tools::failure(&call.id, "deadline_exceeded"),
                root,
                *catching_up,
            ),
            None => tools::admission_failure(&call.id, "deadline_exceeded", &call.attempted),
        };
        tools::result(&call.name, envelope, call.modern)
    };
    // Requests handled at the last fence and not yet decided.
    let mut arrivals: VecDeque<impl_receive::Outcome> = VecDeque::new();
    // Selected calls awaiting their final identity check, and the checks'
    // results not yet taken in (#131).
    let mut checks: HashMap<u64, FinalCheck> = HashMap::new();
    let mut checked: VecDeque<(u64, CheckedResult)> = VecDeque::new();
    loop {
        if arrivals.is_empty() && completed.is_empty() && checked.is_empty() {
            let retry = checks
                .values()
                .filter(|check| !check.running)
                .filter_map(|check| check.retry_at)
                .min();
            let next = in_flight
                .values()
                .filter(|_| workspace.call_timer_enabled())
                .map(|call| call.deadline)
                .chain(retry)
                .min();
            match inbox.wait(next) {
                Some(Input::Frame(frame, relay, accepted, _)) => {
                    if receive(frame, relay, accepted, &mut session, &mut arrivals) {
                        cancel_dropped(&session, &mut in_flight);
                        return Ok(());
                    }
                }
                Some(Input::Done(token, outcome)) => completed.push_back((token, outcome)),
                Some(Input::FinalChecked(token, result)) => checked.push_back((token, result)),
                None if inbox.ended => {
                    session.eof();
                    cancel_dropped(&session, &mut in_flight);
                    return Ok(());
                }
                None => {}
            }
        }
        // Blocking steps come before the fence: each finished call's final
        // root check, and the test pause points. Only a proven missing or
        // replaced root is `root_changed`.
        //
        // A selected call's identity check is split (Decision 0009 §7, owner
        // interpretation 2026-10-19). Its slow part, discovering the common Git
        // directories, runs on its own thread once the call's work finished,
        // so no call's decision waits on another call's lookup. Its comparison
        // runs on the coordinator at the call's send, filesystem calls only
        // (`verify_at_send`), so it is never stale. Not seen at send: a Git
        // pointer re-pointed in place with every root and common-directory
        // inode unchanged, outside ordinary checkout and writes. A lookup that cannot
        // finish by the call's deadline (`Unavailable`, such as a Git
        // subprocess slower than its bound on a loaded host) proves nothing:
        // it is started again after a tick, and the call is
        // `deadline_exceeded` once its deadline passes (#131).
        let mut finished = Vec::new();
        while let Some((token, outcome)) = completed.pop_front() {
            let Some(call) = in_flight.get(&token) else {
                // Already decided at its deadline, or cancelled.
                continue;
            };
            let outcome = outcome?;
            let selected = match &outcome.finals {
                Finals::Selected { witness, .. } if workspace.clock_now() < call.deadline => {
                    Some((witness.clone(), "selected_final"))
                }
                Finals::Capacity { witness, .. } if workspace.clock_now() < call.deadline => {
                    Some((witness.clone(), "capacity_final"))
                }
                _ => None,
            };
            if let Some((witness, stage)) = selected {
                let running = start_final_check(
                    token,
                    &witness,
                    workspace.helper_deadline(call.deadline),
                    stage,
                    &sender,
                );
                let retry_at = (!running).then(|| Instant::now() + FINAL_RECHECK_TICK);
                checks.insert(
                    token,
                    FinalCheck {
                        outcome,
                        running,
                        retry_at,
                    },
                );
                continue;
            }
            let mut result = outcome.result;
            if workspace.clock_now() < call.deadline
                && let Finals::Launch { catching_up } = &outcome.finals
            {
                phase_hook("launch_final")?;
                if workspace.check().is_err() {
                    result = tools::resolved_root_changed(
                        &call.name,
                        &call.id,
                        workspace.root(),
                        *catching_up,
                        call.modern,
                    );
                }
            }
            #[cfg(feature = "test-support")]
            if let Some(hook) = &workspace.hooks.send_decision {
                hook(&call.id);
            }
            finished.push((token, result, None));
        }
        while let Some((token, result)) = checked.pop_front() {
            let result = result?;
            let (Some(mut check), Some(call)) = (checks.remove(&token), in_flight.get(&token))
            else {
                // Its call was decided, cancelled or expired meanwhile.
                continue;
            };
            let (Finals::Selected {
                witness,
                catching_up,
            }
            | Finals::Capacity {
                witness,
                catching_up,
            }) = &check.outcome.finals
            else {
                unreachable!("only selected calls have final checks")
            };
            let gate = match result {
                Ok(dirs) => Some(SendGate {
                    witness: witness.clone(),
                    dirs,
                    catching_up: *catching_up,
                    capacity: matches!(check.outcome.finals, Finals::Capacity { .. }),
                }),
                Err(SelectionError::Unavailable) => {
                    if workspace.clock_now() < call.deadline {
                        check.running = false;
                        check.retry_at = Some(Instant::now() + FINAL_RECHECK_TICK);
                        checks.insert(token, check);
                    }
                    // Past its deadline, the expiry below answers it.
                    continue;
                }
                Err(_) => {
                    check.outcome.result = tools::resolved_root_changed(
                        &call.name,
                        &call.id,
                        &witness.identity.root,
                        *catching_up,
                        call.modern,
                    );
                    None
                }
            };
            #[cfg(feature = "test-support")]
            if let Some(hook) = &workspace.hooks.send_decision {
                hook(&call.id);
            }
            finished.push((token, check.outcome.result, gate));
        }
        let now = Instant::now();
        checks.retain(|token, _| {
            in_flight
                .get(token)
                .is_some_and(|call| workspace.clock_now() < call.deadline)
        });
        for (token, check) in &mut checks {
            if check.running || check.retry_at.is_some_and(|at| now < at) {
                continue;
            }
            let (Finals::Selected { witness, .. } | Finals::Capacity { witness, .. }) =
                &check.outcome.finals
            else {
                unreachable!("only selected calls have final checks")
            };
            let deadline = workspace.helper_deadline(in_flight[token].deadline);
            check.running = start_final_check(*token, witness, deadline, "final_recheck", &sender);
            check.retry_at = (!check.running).then(|| now + FINAL_RECHECK_TICK);
        }
        #[cfg(feature = "test-support")]
        if let Some(hook) = &workspace.hooks.immediate_decision {
            for arrival in &arrivals {
                if let impl_receive::Outcome::Request(admission, _, _) = arrival
                    && matches!(admission.action, session::Action::Call { .. })
                {
                    hook(&admission.id);
                }
            }
        }
        // The fence for every decision below. Requests it handles are decided
        // after the next one.
        let deciding = std::mem::take(&mut arrivals);
        if fence(
            &mut inbox,
            &mut session,
            &mut arrivals,
            &mut completed,
            &mut checked,
        ) {
            for (token, _, _) in &finished {
                if let Some(call) = in_flight.get(token) {
                    call.cancel
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            }
            cancel_dropped(&session, &mut in_flight);
            return Ok(());
        }
        cancel_dropped(&session, &mut in_flight);
        #[cfg(feature = "test-support")]
        if let Some(hook) = &workspace.hooks.fence_handled {
            hook(deciding.len() + arrivals.len());
        }
        // Each decision: cancelled (not pending) -> deadline -> outcome. A
        // selected call's identity is compared again here, at its send, with
        // filesystem calls only: its helper's success may be stale after
        // whatever the coordinator did since, such as writing to a slow
        // client (#131). A comparison that fails with an I/O error proving
        // nothing (`Unavailable`) leaves the call pending: its final check
        // starts again after a tick, until its deadline.
        for (token, result, gate) in finished {
            let Some(call) = in_flight.remove(&token) else {
                continue;
            };
            phase_hook("call_send")?;
            let result = if workspace.clock_now() >= call.deadline {
                deadline_answer(&call)
            } else if let Some(gate) = gate {
                phase_hook("selected_call_send")?;
                match gate.witness.verify_at_send(&gate.dirs) {
                    Ok(()) => result,
                    Err(SelectionError::Unavailable) => {
                        let (witness, catching_up) = (gate.witness, gate.catching_up);
                        let finals = if gate.capacity {
                            Finals::Capacity {
                                witness,
                                catching_up,
                            }
                        } else {
                            Finals::Selected {
                                witness,
                                catching_up,
                            }
                        };
                        checks.insert(
                            token,
                            FinalCheck {
                                outcome: CallOutcome { result, finals },
                                running: false,
                                retry_at: Some(Instant::now() + FINAL_RECHECK_TICK),
                            },
                        );
                        in_flight.insert(token, call);
                        continue;
                    }
                    Err(_) => tools::resolved_root_changed(
                        &call.name,
                        &call.id,
                        &gate.witness.identity.root,
                        gate.catching_up,
                        call.modern,
                    ),
                }
            } else {
                result
            };
            let response = architecture::response(&call.id, result);
            if let Some(value) = session
                .prepare(token, response)
                .and_then(|prepared| session.commit(prepared))
            {
                wire::write_relayed_response(&mut writer, call.relay, &value)?;
            }
        }
        let now = workspace.clock_now();
        let mut expired: Vec<u64> = in_flight
            .iter()
            .filter(|(_, call)| call.deadline <= now)
            .map(|(token, _)| *token)
            .collect();
        expired.sort_unstable();
        for token in expired {
            let call = in_flight.remove(&token).expect("expired call");
            call.cancel
                .store(true, std::sync::atomic::Ordering::Release);
            let response = architecture::response(&call.id, deadline_answer(&call));
            if let Some(value) = session
                .prepare(token, response)
                .and_then(|prepared| session.commit(prepared))
            {
                wire::write_relayed_response(&mut writer, call.relay, &value)?;
            }
        }
        let mut non_calls = 0;
        for arrival in deciding {
            let (admission, accepted, relay) = match arrival {
                impl_receive::Outcome::Error(error, relay) => {
                    // Never admitted, so no cancel can name it.
                    wire::write_relayed_response(&mut writer, relay, &error.response())?;
                    continue;
                }
                impl_receive::Outcome::Request(admission, accepted, relay) => {
                    (admission, accepted, relay)
                }
            };
            if session.pending(admission.token).is_none() {
                // Cancelled at the fence.
                continue;
            }
            let modern = session.mode() == session::Mode::Modern;
            let deadline = accepted + workspace.call_deadline;
            let result = match &admission.action {
                session::Action::Unknown => None,
                session::Action::Call { name, arguments } => {
                    let attempted = attempted_workspace(arguments.as_ref(), &workspace);
                    let refuse = |code: &str| {
                        Some(tools::result(
                            name,
                            tools::admission_failure(&admission.id, code, &attempted),
                            modern,
                        ))
                    };
                    if workspace.clock_now() >= deadline {
                        // The deadline overrides any outcome still unsent.
                        refuse("deadline_exceeded")
                    } else if name != architecture::PROMPT_NAME
                        && tools::validate(name, arguments.as_ref()).is_err()
                    {
                        // Application validation precedes capacity.
                        Some(
                            tools::prepare(
                                name,
                                arguments.as_ref(),
                                &admission.id,
                                modern,
                                &workspace,
                            )
                            .response,
                        )
                    } else if live_calls(&session, &in_flight) >= MAX_CONNECTION_CALLS {
                        refuse("too_many_requests")
                    } else if let Some(permit) = WorkerPermit::acquire() {
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let resolved: Resolved = Default::default();
                        let context = CallContext {
                            workspace: workspace.clone(),
                            registry: registry.clone(),
                            daemon_handle: daemon_handle.clone(),
                            id: admission.id.clone(),
                            modern,
                            name: name.clone(),
                            arguments: arguments.clone(),
                            deadline,
                            cancel: cancel.clone(),
                            resolved: resolved.clone(),
                        };
                        let done = sender.clone();
                        let token = admission.token;
                        #[cfg(feature = "test-support")]
                        let worker_done = workspace.hooks.worker_done.clone();
                        let spawned =
                            std::thread::Builder::new()
                                .name("mcp-call".into())
                                .spawn(move || {
                                    let _permit = permit;
                                    let outcome = run_call(context);
                                    let _ = done.send(Input::Done(token, outcome));
                                    #[cfg(feature = "test-support")]
                                    if let Some(hook) = worker_done {
                                        hook();
                                    }
                                });
                        match spawned {
                            Ok(_) => {
                                in_flight.insert(
                                    token,
                                    InFlight {
                                        id: admission.id.clone(),
                                        name: name.clone(),
                                        relay,
                                        modern,
                                        deadline,
                                        cancel,
                                        resolved,
                                        attempted,
                                    },
                                );
                                // Dispatched: decided when it finishes.
                                continue;
                            }
                            // The permit went with the failed closure.
                            Err(_) => refuse("too_many_requests"),
                        }
                    } else {
                        refuse("too_many_requests")
                    }
                }
                action => {
                    non_calls += 1;
                    if non_calls > MAX_CONNECTION_REQUESTS {
                        let response = serde_json::json!({"jsonrpc":"2.0","id":admission.id,
                            "error":{"code":-32000,"message":"Too many outstanding requests",
                                "data":{"code":"too_many_requests"}}});
                        if let Some(value) = session
                            .prepare(admission.token, response)
                            .and_then(|prepared| session.commit(prepared))
                        {
                            wire::write_relayed_response(&mut writer, relay, &value)?;
                        }
                        continue;
                    }
                    Some(match action {
                        session::Action::Discover => catalog::discover(workspace.build_version()),
                        session::Action::Initialize => {
                            catalog::initialize(workspace.build_version())
                        }
                        session::Action::PromptList => architecture::list(modern),
                        _ => catalog::list(modern),
                    })
                }
            };
            let response = match result {
                Some(result) => architecture::response(&admission.id, result),
                None => wire::ProtocolError::new(-32601, admission.id.clone()).response(),
            };
            // Only the static catalog may use its larger cap.
            let cap = if matches!(admission.action, session::Action::List) {
                wire::CATALOG_BYTES
            } else {
                wire::RESPONSE_BYTES
            };
            if let Some(value) = session
                .prepare(admission.token, response)
                .and_then(|prepared| session.commit(prepared))
            {
                wire::write_relayed_capped(&mut writer, relay, &value, cap)?;
            }
        }
    }
}

/// Live calls on this connection: dispatched and still pending. The one place
/// call capacity is counted (Decision 0009 §7); a cancel frees it at once.
fn live_calls(
    session: &session::Session,
    in_flight: &std::collections::HashMap<u64, InFlight>,
) -> usize {
    in_flight
        .keys()
        .filter(|token| session.pending(**token).is_some())
        .count()
}

/// The workspace a call attempted: its explicit selection, else the launch
/// workspace.
fn attempted_workspace(
    arguments: Option<&serde_json::Value>,
    workspace: &OpenedWorkspace,
) -> serde_json::Value {
    arguments
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("workspace"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!(workspace.root()))
}

fn receive(
    frame: wire::Frame,
    relay: Option<u64>,
    accepted: std::time::Instant,
    session: &mut session::Session,
    outcomes: &mut std::collections::VecDeque<impl_receive::Outcome>,
) -> bool {
    if frame == wire::Frame::Eof {
        session.eof();
        return true;
    }
    match wire::decode(frame) {
        Err(error) => outcomes.push_back(impl_receive::Outcome::Error(error, relay)),
        Ok(None) => {
            session.eof();
            return true;
        }
        Ok(Some(request)) => match session.accept(request, catalog::known) {
            session::Event::Error(error) => {
                outcomes.push_back(impl_receive::Outcome::Error(error, relay))
            }
            // The call's deadline starts at protocol acceptance (Decision 0009 §7).
            session::Event::Admitted(admission) => {
                outcomes.push_back(impl_receive::Outcome::Request(admission, accepted, relay))
            }
            session::Event::Ignore => (),
        },
    }
    false
}
mod impl_receive {
    pub(super) enum Outcome {
        /// A protocol error, and the relay ID of its frame on the relayed transport.
        Error(super::wire::ProtocolError, Option<u64>),
        /// An admitted request, the instant it was accepted, and its relay ID.
        Request(super::session::Admission, std::time::Instant, Option<u64>),
    }
}

#[cfg(test)]
mod w1_error_tests {
    #[test]
    fn typed_busy_projects_only_an_advertised_retryable_code() {
        let error = anyhow::Error::new(crate::store::topology::StorageBusy);
        let code = super::mcp_read_error(&error);
        assert_eq!(code, "deadline_exceeded");
        let catalog = super::catalog::list(true);
        let codes = catalog["tools"][0]["outputSchema"]["$defs"]["ResolvedFailureCode"]["enum"]
            .as_array()
            .unwrap();
        assert!(codes.iter().any(|item| item == code));
        assert!(!codes.iter().any(|item| item == "storage_busy"));
        let resolved = super::tools::failure(&serde_json::json!(1), code);
        assert_eq!(resolved["error"]["retryable"], true);
    }

    #[test]
    fn rebuild_in_progress_is_an_advertised_retryable_code() {
        let rebuild = crate::store::RebuildInProgress {
            since: std::time::SystemTime::now(),
            documents: 10_000,
        };
        let error = anyhow::Error::new(rebuild).context("selected read");
        let code = super::mcp_read_error(&error);
        assert_eq!(code, "rebuild_in_progress");
        let catalog = super::catalog::list(true);
        let codes = catalog["tools"][0]["outputSchema"]["$defs"]["ResolvedFailureCode"]["enum"]
            .as_array()
            .unwrap();
        assert!(codes.iter().any(|item| item == code));
        let resolved = super::tools::failure(&serde_json::json!(1), code);
        assert_eq!(resolved["error"]["retryable"], true);
        assert!(!crate::store::cli_read_retryable_contention(&error));
    }
}

#[cfg(test)]
mod settlement_tests {
    use super::*;

    /// Decision 0009 §7: a decision settles exactly the frames completed
    /// before it. A later frame, even one already published, is kept in order
    /// for the next decision.
    #[test]
    fn settlement_takes_only_frames_completed_by_the_decision() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let window = std::sync::Arc::new(FrameWindow::default());
        window.acquire();
        window.acquire();
        let mut inbox = Inbox {
            receiver,
            window,
            completed: completed.clone(),
            received: 0,
            later: None,
            ended: false,
            #[cfg(feature = "test-support")]
            waiting_hook: None,
        };
        let now = std::time::Instant::now();
        let frame = |seq| Input::Frame(wire::Frame::Line(b"{}".to_vec()), Some(seq), now, seq);
        sender.send(frame(1)).unwrap();
        let decision = inbox.decision();
        // Frame 2 completes and is published after the decision.
        completed.store(2, std::sync::atomic::Ordering::Release);
        sender.send(frame(2)).unwrap();
        assert!(matches!(
            inbox.settled(decision),
            Some(Input::Frame(_, Some(1), _, 1))
        ));
        assert!(
            inbox.settled(decision).is_none(),
            "settled past the decision"
        );
        assert!(matches!(
            inbox.settled(inbox.decision()),
            Some(Input::Frame(_, Some(2), _, 2))
        ));
    }
}
