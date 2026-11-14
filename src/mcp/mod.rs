//! Bounded read-only MCP protocol and single-connection stdio transport.
pub mod architecture;
pub mod catalog;
#[cfg(test)]
mod find_trigram_tests;
mod projection;
mod readers;
pub mod search;
pub mod session;
pub mod show;
pub mod tools;
pub mod wire;

use crate::daemon::registry::SelectionError;
use crate::store::topology::WorkspaceIdentity;
use crate::testing::phase_hook;
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
    /// A fixture's call-worker budget, in place of the process-wide one.
    #[cfg(feature = "test-support")]
    call_workers: Option<CallWorkersForTests>,
}

/// A test hook given a call's JSON-RPC ID.
#[cfg(feature = "test-support")]
pub type CallHookForTests = std::sync::Arc<dyn Fn(&serde_json::Value) + Send + Sync>;

/// A test hook given a finished call's ID and its committed response (#930).
#[cfg(feature = "test-support")]
pub type CommittedResponseHook =
    std::sync::Arc<dyn Fn(&serde_json::Value, &mut serde_json::Value) + Send + Sync>;

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
    /// On the coordinator, given a finished call's ID and its committed JSON-RPC
    /// response just before it is written; a test may enlarge it past the cap (#930).
    pub committed_response: Option<CommittedResponseHook>,
    /// This connection's phase barrier and `git`, on the coordinator and every
    /// thread it starts, in place of process environment (#869).
    pub scope: crate::testing::FixtureScope,
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
            #[cfg(feature = "test-support")]
            call_workers: None,
        }
    }
    /// Count this connection's call workers against `budget`, which only the
    /// fixture's own connections share, in place of the process-wide budget
    /// (#869).
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn with_call_workers_for_tests(mut self, budget: CallWorkersForTests) -> Self {
        self.call_workers = Some(budget);
        self
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
    /// The call-worker budget this connection's workers count against: a
    /// fixture's, or (`None`) the process-wide one.
    fn call_workers(&self) -> Option<std::sync::Arc<std::sync::atomic::AtomicUsize>> {
        #[cfg(feature = "test-support")]
        if let Some(budget) = &self.call_workers {
            return Some(budget.0.clone());
        }
        None
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

    /// #741: an idle daemon MCP session ends on the published shutdown while
    /// its client stays connected. The socket still carries data afterwards,
    /// so no socket shutdown ended the session.
    #[tokio::test(flavor = "current_thread")]
    async fn published_shutdown_ends_an_idle_daemon_session_without_a_socket_shutdown() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("selected");
        std::fs::create_dir(&root).unwrap();
        let workspace =
            OpenedWorkspace::new(WorkspaceIdentity::discover(Some(&root), temp.path()).unwrap());
        let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
        let probe = server.try_clone().unwrap();
        let (shutdown, stopped) = tokio::sync::watch::channel(false);
        let (parked, waiting) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Handle::current();
        let session = tokio::task::spawn_blocking(move || {
            run_daemon_socket_hooked(
                workspace,
                server,
                false,
                stopped,
                None,
                runtime,
                Some(Box::new(move || {
                    let _ = parked.send(());
                })),
            )
        });
        tokio::task::spawn_blocking(move || waiting.recv_timeout(crate::testing::HANG_BACKSTOP))
            .await
            .unwrap()
            .expect("idle session reader never parked");
        shutdown.send_replace(true);
        let ended = tokio::time::timeout(crate::testing::HANG_BACKSTOP, session)
            .await
            .expect("published shutdown did not end the idle MCP session")
            .unwrap();
        assert!(ended.is_ok(), "{ended:?}");
        use std::io::{Read, Write};
        (&client).write_all(&[7]).unwrap();
        let mut byte = [0];
        (&probe).read_exact(&mut byte).unwrap();
        assert_eq!(byte, [7], "the session was ended by a socket shutdown");
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

/// Run the same protocol coordinator on a daemon-owned session socket. Its
/// reader ends when this call returns.
pub fn run_socket(
    workspace: OpenedWorkspace,
    stream: std::os::unix::net::UnixStream,
    registry: std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    session_id: u64,
    daemon_handle: tokio::runtime::Handle,
) -> std::io::Result<()> {
    let (_session, ended) = tokio::sync::watch::channel(false);
    run_daemon_socket(
        workspace,
        stream,
        false,
        ended,
        registry,
        session_id,
        daemon_handle,
    )
}

/// Run the coordinator on a daemon session socket whose reader also ends,
/// as at end of input, when the daemon publishes `shutdown` (#741). With
/// `relayed`, this is the daemon side of `trellis mcp`'s relay (Decision 0009
/// §7): every frame and reply carries the relay's own ID before the unchanged
/// MCP line.
pub fn run_daemon_socket(
    workspace: OpenedWorkspace,
    stream: std::os::unix::net::UnixStream,
    relayed: bool,
    shutdown: tokio::sync::watch::Receiver<bool>,
    registry: std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    session_id: u64,
    daemon_handle: tokio::runtime::Handle,
) -> std::io::Result<()> {
    run_daemon_socket_hooked(
        workspace,
        stream,
        relayed,
        shutdown,
        Some((registry, session_id)),
        daemon_handle,
        None,
    )
}

fn run_daemon_socket_hooked(
    workspace: OpenedWorkspace,
    stream: std::os::unix::net::UnixStream,
    relayed: bool,
    shutdown: tokio::sync::watch::Receiver<bool>,
    registry: Registry,
    daemon_handle: tokio::runtime::Handle,
    waiting: Option<crate::daemon::shutdown_read::WaitHookForTests>,
) -> std::io::Result<()> {
    let mut reader =
        crate::daemon::shutdown_read::ShutdownReader::new(stream.try_clone()?, shutdown)?;
    if let Some(waiting) = waiting {
        reader = reader.on_wait_for_tests(waiting);
    }
    run_transport(
        workspace,
        reader,
        stream,
        registry,
        Some(daemon_handle),
        relayed,
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

/// An explicit `semanticProducerId` the admitted revision does not publish (#467), typed
/// as `trellis_call_graph` types its unpublished producer.
fn producer_failure(id: &serde_json::Value) -> serde_json::Value {
    let mut envelope = tools::failure(id, "not_found");
    envelope["error"]["message"] =
        serde_json::json!("Semantic producer not published for the admitted revision");
    envelope
}

/// The files of the apps a find, search or call graph `app` argument names (#851), or the
/// typed failure that answers instead: `not_found` for an unknown app, and the rules check's
/// `invalid_rules` and `bounded_work`. Contention and a building core return as errors, so
/// the read waits for the owner and is admitted again.
fn app_files(
    runtime: &crate::daemon::registry::CheckoutRuntime,
    response: &crate::daemon::registry::CheckoutEvidence,
    id: &serde_json::Value,
    app: Option<&str>,
    check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
) -> anyhow::Result<AppFiles> {
    let Some(app) = app else {
        return Ok(Ok(None));
    };
    match runtime.mcp_app_files(response, app, check) {
        Ok(files) => Ok(Ok(Some(std::sync::Arc::new(files)))),
        Err(error)
            if crate::store::cli_read_retryable_contention(&error)
                || error
                    .downcast_ref::<crate::http::architecture::ArchitectureBuilding>()
                    .is_some() =>
        {
            Err(error)
        }
        Err(error) => {
            let message = error.to_string();
            let code = if crate::rules::diagnostic(&error).is_some() {
                "invalid_rules"
            } else if message.starts_with("not_found:") {
                "not_found"
            } else if message.starts_with("bounded_work") {
                "bounded_work"
            } else {
                evidence_failure_code(&error)
            };
            let mut envelope = tools::failure(id, code);
            if code == "not_found" {
                envelope["error"]["message"] = serde_json::json!(crate::rules::UNKNOWN_APP);
            }
            Ok(Err(envelope))
        }
    }
}

/// An `app` filter's files (`None` without one), or the failure that answers the call.
type AppFiles =
    Result<Option<std::sync::Arc<std::collections::BTreeSet<String>>>, serde_json::Value>;

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

/// Resolve a selector only inside the operation's admitted snapshot.
#[allow(clippy::too_many_arguments)]
fn lookup_root(
    response: &crate::daemon::registry::CheckoutEvidence,
    id: &serde_json::Value,
    name: &str,
    key: &str,
    expected: Option<crate::model::IndexPin>,
    root: &std::path::Path,
    sampled: bool,
    modern: bool,
) -> anyhow::Result<Result<String, serde_json::Value>> {
    let found = response.mcp_resolve_lookup_with_lines(key, expected)?;
    match found.symbols.as_slice() {
        [] => Ok(Err(tools::failure(id, "not_found"))),
        [symbol] => Ok(Ok(symbol.symbol.declaration.syntax_id.clone())),
        _ => {
            let tiers = response.mcp_language_tiers()?;
            let envelope =
                projection::lookup_candidates(id, name, key, found, &tiers, root, sampled, modern)
                    .map_err(|error| anyhow::anyhow!(error))?
                    .unwrap_or_else(|| tools::failure(id, "range_too_large"));
            Ok(Err(envelope))
        }
    }
}
/// Work of a tool before read admission, outside any read transaction.
type BeforeEvidenceRead<'a> = dyn Fn(&crate::daemon::registry::CheckoutRuntime) + 'a;

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
    before_read: Option<&BeforeEvidenceRead<'_>>,
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
    if let Some(before_read) = before_read {
        before_read(&runtime);
    }
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
                        // No query embedding without a resident matrix. The
                        // budget still bounds a pending head's cache count.
                        let mut query = search.unavailable_query(
                            crate::search_model::SearchUnavailableReason::MatrixNotResident,
                        );
                        query.budget = Some(budget);
                        return Ok(query);
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
                Err(error)
                    if crate::store::cli_read_retryable_contention(&error)
                        || error
                            .downcast_ref::<crate::http::architecture::ArchitectureBuilding>()
                            .is_some() =>
                {
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
            // The head moved, or the first wait could not admit. The read has ended, so this
            // wait holds nothing; the next attempt answers from what the owner published.
            Err(ref error)
                if error
                    .downcast_ref::<crate::http::architecture::ArchitectureBuilding>()
                    .is_some() =>
            {
                if let Some(before_read) = before_read {
                    before_read(&runtime);
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            answer => return answer,
        }
    }
}

struct DescribedSnapshot {
    data: Option<(crate::model::IndexStatus, projection::LanguageCounts)>,
    /// The failed and partial semantic documents, when the call asked for them (#780).
    coverage_documents: Option<Vec<crate::store::CoverageDocument>>,
    /// Head retrieval coverage from the same selected snapshot (#777).
    search_head: Option<crate::search_model::SearchHeadCoverage>,
    catching_up: bool,
    state: &'static str,
    progress: Option<serde_json::Value>,
    /// #851: the head's apps, from its architecture core when the owner has it.
    apps: Option<serde_json::Value>,
}

/// Every tool call is answered within this deadline; describe reports it as
/// `limits.deadlineMs` (contract W1).
pub const CALL_DEADLINE: std::time::Duration = std::time::Duration::from_millis(5000);
/// An architecture read stops waiting for the owner this long before its deadline.
const ARCHITECTURE_ANSWER_TIME: std::time::Duration = std::time::Duration::from_secs(1);
/// A read held out by a spilled publication stops waiting for its COMMIT this long before
/// the call's deadline, and answers `rebuild_in_progress` (#905).
const SPILL_ANSWER_TIME: std::time::Duration = std::time::Duration::from_millis(500);

/// Outgoing and incoming calls emit this many items unless `limit` says otherwise (#528):
/// six semantic bindings with their proofs measured about 23.5 KB on this repository's
/// wide caller list (`index_workspace`); twenty `compact` callers measured about 18 KB.
pub const CALL_DEFAULT_LIMIT: usize = 6;

/// Keep mandatory lexical/projection work inside the original call deadline.
const SEARCH_LEXICAL_RESERVE: std::time::Duration = std::time::Duration::from_millis(2000);

/// Retry only direct Store contention. Every failed attempt drops its selected
/// response; the one successful answer is materialized and finished in one snapshot.
fn read_describe(
    runtime: &crate::daemon::registry::CheckoutRuntime,
    selected: Option<&crate::daemon::registry::SelectedCheckout>,
    coverage_documents: Option<Option<&str>>,
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
                    let read = response
                        .map(|response| {
                            let (data, documents) = (|| {
                                let status = response.status()?;
                                let counts = response.mcp_language_counts()?;
                                let tiers = response.mcp_language_tiers()?;
                                let documents = coverage_documents
                                    .map(|language| {
                                        response.mcp_coverage_documents(&tiers, language)
                                    })
                                    .transpose()?;
                                Ok(((status, (counts, tiers)), documents))
                            })()
                            .map_err(|error| response.report_index_failure(error))?;
                            let mut check = || {
                                if cancel.load(std::sync::atomic::Ordering::Acquire)
                                    || runtime.mcp_clock_now() >= deadline
                                {
                                    anyhow::bail!(
                                        "deadline_exceeded: MCP call reached its deadline"
                                    );
                                }
                                Ok(())
                            };
                            let search_head = runtime.search_head_coverage(
                                &response,
                                data.0.revision,
                                &mut check,
                            )?;
                            let apps = runtime.mcp_describe_apps(&response, &mut check)?;
                            response.finish((data, documents, search_head, apps))
                        })
                        .transpose()?;
                    let (data, documents, search_head, apps) = match read {
                        Some((data, documents, search_head, apps)) => {
                            (Some(data), documents, search_head, Some(apps))
                        }
                        None => (None, None, None, None),
                    };
                    Ok(DescribedSnapshot {
                        data,
                        coverage_documents: documents,
                        search_head,
                        catching_up,
                        state,
                        progress,
                        apps,
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

/// Call workers alive across all connections, each holding a permit until
/// its thread exits, even after its call was decided (Decision 0009 §7).
static CALL_WORKERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
const MAX_CALL_WORKERS: usize = 16;
/// Outstanding calls per connection: a fifth is refused without queueing.
pub(crate) const MAX_CONNECTION_CALLS: usize = 4;

/// A call-worker budget that a fixture's connections share in place of the
/// process-wide one (#869), so a test of the budget neither starves nor waits
/// on the other tests in its test binary.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[derive(Clone, Debug, Default)]
pub struct CallWorkersForTests(std::sync::Arc<std::sync::atomic::AtomicUsize>);

#[cfg(feature = "test-support")]
impl CallWorkersForTests {
    /// Call workers alive on this budget.
    pub fn live(&self) -> usize {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// A live call worker's share of its budget: the process-wide one, or a
/// fixture's.
struct WorkerPermit(Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>);
impl WorkerPermit {
    fn acquire(budget: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>) -> Option<Self> {
        use std::sync::atomic::Ordering;
        let workers = budget.as_deref().unwrap_or(&CALL_WORKERS);
        let mut live = workers.load(Ordering::Acquire);
        while live < MAX_CALL_WORKERS {
            match workers.compare_exchange_weak(live, live + 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return Some(WorkerPermit(budget)),
                Err(current) => live = current,
            }
        }
        None
    }
    fn workers(&self) -> &std::sync::atomic::AtomicUsize {
        self.0.as_deref().unwrap_or(&CALL_WORKERS)
    }
}
impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.workers()
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
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
    let scope = crate::testing::inherit_scope();
    std::thread::Builder::new()
        .name("mcp-final-check".into())
        .spawn(move || {
            let _scope = crate::testing::enter_scope(scope);
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
    /// A `trellis_show` target its read checked (#818). The coordinator records it only at
    /// the call's final successful send decision, so a call that answers any failure records
    /// nothing.
    show: Option<show::Pending>,
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
                        show: None,
                    });
                }
                Ok(selected_workspace) => selected_workspace,
            };
            let answering = selected_workspace.as_ref().unwrap_or(&workspace);
            let mut tool = tools::prepare(&name, arguments.as_ref(), &id, modern, answering);
            let mut sampled_catching_up = catching_up;
            let mut envelope = tool.response["structuredContent"].clone();
            let mut pending_coverage = None;
            if name == "trellis_workspace_describe"
                && tool.valid_arguments
                && let Some((registry, _)) = &registry
            {
                // #780: the failed and partial document page the call asked for.
                let coverage_request = arguments
                    .as_ref()
                    .and_then(|arguments| arguments.get("coverageDocuments"));
                let answering_root = answering.root().to_owned();
                let publish = |catching_up: bool| {
                    *resolved.lock().unwrap() = Some((answering_root.clone(), catching_up));
                };
                match describe(
                    registry,
                    key,
                    DescribeTarget {
                        selected: selected.as_ref(),
                        coverage_documents: coverage_request
                            .map(|request| request["language"].as_str()),
                    },
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
                        let mut search_head = None;
                        match read {
                            Ok(DescribedSnapshot {
                                data: Some((status, counts)),
                                coverage_documents,
                                search_head: head,
                                catching_up: sampled,
                                state,
                                progress,
                                apps,
                            }) => {
                                search_head = head;
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
                                if let Some(apps) = apps {
                                    envelope["data"]["apps"] = apps;
                                }
                                // Paged last, against the final answer (#780).
                                if let (Some(request), Some(documents)) =
                                    (coverage_request, coverage_documents)
                                {
                                    pending_coverage = Some((documents, request));
                                }
                            }
                            Ok(DescribedSnapshot {
                                data: None,
                                coverage_documents: _,
                                search_head: _,
                                catching_up: sampled,
                                state,
                                progress,
                                apps: _,
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
                                envelope["data"]["searchIndex"] = serde_json::to_value(
                                    search_availability.with_head(search_head),
                                )?;
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
            // The target `trellis_show` checked in its admitted read, recorded once that read
            // has finished (#818). Each read attempt sets or clears it.
            let show_target: std::sync::Mutex<
                Option<(
                    crate::daemon::view_request::ViewTarget,
                    crate::model::IndexPin,
                )>,
            > = std::sync::Mutex::new(None);
            let show_target = &show_target;
            let mut pending_show = None;
            let cx = readers::ReadCx {
                id,
                root,
                modern,
                deadline,
                accepted,
                cancel,
                show_target,
            };
            let tool_read: Option<EvidenceToolRead> = match (name.as_str(), args) {
                _ if !tool.valid_arguments || envelope["error"]["code"] != "index_not_ready" => {
                    None
                }
                (name, Some(args))
                    if architecture::is_tool(name) || name == architecture::PROMPT_NAME =>
                {
                    readers::architecture_read(cx, name, args)
                }
                ("trellis_search_symbols", Some(args)) => search::search_read(cx, args),
                ("trellis_find_symbols", Some(args)) => readers::find_read(cx, args),
                ("trellis_declaration", Some(args)) => readers::declaration_read(cx, args),
                ("trellis_outgoing_calls", Some(args)) => readers::outgoing_read(cx, args),
                ("trellis_incoming_calls", Some(args)) => readers::incoming_read(cx, args),
                ("trellis_call_graph", Some(args)) => readers::call_graph_read(cx, args),
                (show::NAME, Some(args)) => readers::show_read(cx, args),
                ("trellis_read_source", Some(args)) => readers::source_read(cx, args),
                _ => None,
            };
            if let Some(tool_read) = tool_read
                && let Some((registry, _)) = &registry
            {
                // An architecture read waits for the owner, leaving its deadline's last
                // second for the answer.
                let architecture_read =
                    architecture::is_tool(&name) || name == architecture::PROMPT_NAME;
                // #851: a find, search or call graph `app` filter reads the head's core too.
                let app_filtered =
                    !architecture_read && args.is_some_and(|args| args.contains_key("app"));
                let await_architecture = |runtime: &crate::daemon::registry::CheckoutRuntime| {
                    let store_arguments = architecture::store_arguments(
                        &name,
                        arguments.as_ref().unwrap_or(&serde_json::Value::Null),
                    );
                    let wait = deadline
                        .saturating_duration_since(runtime.mcp_clock_now())
                        .saturating_sub(ARCHITECTURE_ANSWER_TIME);
                    runtime.await_architecture_tool(
                        if architecture_read {
                            architecture::operation(&name)
                        } else {
                            "apps"
                        },
                        &store_arguments,
                        wait,
                        &mut || {
                            anyhow::ensure!(
                                !cancel.load(std::sync::atomic::Ordering::Acquire),
                                "cancelled"
                            );
                            Ok(())
                        },
                    );
                };
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
                    (architecture_read || app_filtered)
                        .then_some(&await_architecture as &BeforeEvidenceRead<'_>),
                    &*tool_read,
                ) {
                    Ok((found, sampled)) => {
                        envelope = found;
                        sampled_catching_up = sampled;
                        // Only a finished read's target can be shown; the coordinator
                        // records it when it decides to send this answer.
                        let checked = show_target.lock().unwrap().take();
                        if let Some((target, pin)) = checked
                            && envelope.get("error").is_none()
                        {
                            pending_show = Some(show::Pending {
                                runtime: registry.blocking_lock().runtime(key),
                                target,
                                pin,
                            });
                        }
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
                                show: None,
                            });
                        }
                        envelope = tools::failure(id, code);
                    }
                }
            }
            *resolved.lock().unwrap() = Some((answering.root().to_owned(), sampled_catching_up));
            envelope["workspace"] = serde_json::json!(answering.root());
            envelope["catchingUp"] = serde_json::json!(sampled_catching_up);
            if let Some((documents, request)) = pending_coverage
                && envelope.get("data").is_some()
            {
                envelope = projection::coverage_documents(id, envelope, documents, request, modern);
            }
            // #930: describe fits its final answer like the other tools; one over the
            // response cap is that call's typed failure.
            if name == "trellis_workspace_describe"
                && envelope.get("data").is_some()
                && projection::line_bytes(id, &envelope, modern) > wire::RESPONSE_BYTES
            {
                envelope = tools::response_too_large(id, &envelope);
            }
            tool.response = if architecture::is_tool(&name) || name == architecture::PROMPT_NAME {
                // Tools page their arrays into the response cap; the prompt never pages.
                let paging = architecture::is_tool(&name)
                    .then(|| architecture::Paging::from_arguments(&name, arguments.as_ref()));
                architecture::capped(id, &name, envelope, paging, modern)
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
                show: pending_show,
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
                        show: None,
                    })
                }
                Err(error) => Ok(CallOutcome {
                    result: tools::result(
                        &name,
                        tools::selection_failure(&id, error, &serde_json::json!(path)),
                        modern,
                    ),
                    finals: Finals::None,
                    show: None,
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
                show: None,
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

/// What one describe call reads: the selected worktree, if any, and the coverage document
/// language filter when the call asked for documents (#780).
struct DescribeTarget<'a> {
    selected: Option<&'a crate::daemon::registry::SelectedCheckout>,
    coverage_documents: Option<Option<&'a str>>,
}

fn describe(
    registry: &std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    key: &str,
    target: DescribeTarget<'_>,
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
    let read = read_describe(
        &runtime,
        target.selected,
        target.coverage_documents,
        deadline,
        cancel,
        admitted,
    );
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
    let mut connection =
        Connection::open(workspace, input, output, registry, daemon_handle, relayed)?;
    loop {
        if connection.wait_idle() {
            return Ok(());
        }
        let mut finished = connection.settle_completed()?;
        connection.settle_checked(&mut finished)?;
        connection.restart_due_checks();
        let Some(deciding) = connection.fence_decisions(&finished) else {
            return Ok(());
        };
        connection.send_finished(finished)?;
        connection.expire()?;
        connection.admit(deciding)?;
    }
}

/// A finished call's decision awaiting its send: its token, its result, its send-time
/// identity gate if it selected a checkout, and the show to record with a success.
type Finished = (
    u64,
    serde_json::Value,
    Option<SendGate>,
    Option<show::Pending>,
);

/// One connection's coordinator state (Decision 0009 §7). `coordinate` runs its phases in
/// order, once per turn: `wait_idle`, `settle_completed`, `settle_checked`,
/// `restart_due_checks`, `fence_decisions`, `send_finished`, `expire`, `admit`.
///
/// The fields are declared in the order they are dropped when the connection ends: a
/// dropped `InFlight` cancels its worker, and the dropped inbox ends the reader.
struct Connection<W: std::io::Write> {
    /// The final checks' results not yet taken in (#131).
    checked: std::collections::VecDeque<(u64, CheckedResult)>,
    /// Selected calls awaiting their final identity check (#131).
    checks: std::collections::HashMap<u64, FinalCheck>,
    /// Requests handled at the last fence and not yet decided.
    arrivals: std::collections::VecDeque<impl_receive::Outcome>,
    writer: W,
    in_flight: std::collections::HashMap<u64, InFlight>,
    /// Worker results not yet settled.
    completed: std::collections::VecDeque<(u64, std::io::Result<CallOutcome>)>,
    session: session::Session,
    inbox: Inbox,
    sender: std::sync::mpsc::Sender<Input>,
    workspace: std::sync::Arc<OpenedWorkspace>,
    #[cfg(feature = "test-support")]
    _scope: crate::testing::ScopeGuard,
    daemon_handle: Option<tokio::runtime::Handle>,
    registry: Registry,
}

impl<W: std::io::Write> Connection<W> {
    /// Starts the `mcp-stdin` reader thread and returns the connection, with nothing
    /// received yet.
    fn open(
        workspace: OpenedWorkspace,
        input: impl std::io::Read + Send + 'static,
        output: W,
        registry: Registry,
        daemon_handle: Option<tokio::runtime::Handle>,
        relayed: bool,
    ) -> std::io::Result<Self> {
        use std::collections::{HashMap, VecDeque};
        use std::io::BufReader;
        #[cfg(feature = "test-support")]
        let _scope = crate::testing::enter_scope(Some(workspace.hooks.scope.clone()));
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
                            let _ = reader_sender.send(Input::Frame(
                                wire::Frame::Eof,
                                None,
                                accepted,
                                seq,
                            ));
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
                            let _ = writeln!(
                                std::io::stderr().lock(),
                                "MCP stdin read failed: {error}"
                            );
                            reader_window.acquire();
                            let _ = reader_sender.send(Input::Frame(
                                wire::Frame::Eof,
                                None,
                                accepted,
                                seq,
                            ));
                            break;
                        }
                    }
                }
            })?;
        let inbox = Inbox {
            receiver,
            window,
            completed,
            received: 0,
            later: None,
            ended: false,
            #[cfg(feature = "test-support")]
            waiting_hook: workspace.hooks.settle_waiting.clone(),
        };
        let session = session::Session::new();
        let completed: VecDeque<(u64, std::io::Result<CallOutcome>)> = VecDeque::new();
        let in_flight: HashMap<u64, InFlight> = HashMap::new();
        let writer = output;
        // Requests handled at the last fence and not yet decided.
        let arrivals: VecDeque<impl_receive::Outcome> = VecDeque::new();
        // Selected calls awaiting their final identity check, and the checks'
        // results not yet taken in (#131).
        let checks: HashMap<u64, FinalCheck> = HashMap::new();
        let checked: VecDeque<(u64, CheckedResult)> = VecDeque::new();
        Ok(Self {
            checked,
            checks,
            arrivals,
            writer,
            in_flight,
            completed,
            session,
            inbox,
            sender,
            workspace,
            #[cfg(feature = "test-support")]
            _scope,
            daemon_handle,
            registry,
        })
    }

    /// One fence pass (Decision 0009 §7): handle every frame completed before
    /// now, in order. A cancel removes its ID's pending admission; a request
    /// joins `arrivals`, decided after a later fence; a worker result joins
    /// `completed`. Nothing is written here. Returns true at end of input.
    fn fence(&mut self) -> bool {
        let until = self.inbox.decision();
        while let Some(input) = self.inbox.settled(until) {
            match input {
                Input::Frame(frame, relay, accepted, _) => {
                    if receive(
                        frame,
                        relay,
                        accepted,
                        &mut self.session,
                        &mut self.arrivals,
                    ) {
                        return true;
                    }
                }
                Input::Done(token, outcome) => self.completed.push_back((token, outcome)),
                Input::FinalChecked(token, result) => self.checked.push_back((token, result)),
            }
        }
        if self.inbox.ended {
            self.session.eof();
            return true;
        }
        false
    }

    /// A cancelled or ended call stops its worker cooperatively.
    fn cancel_dropped(&mut self) {
        self.in_flight.retain(|token, call| {
            let pending = self.session.pending(*token).is_some();
            if !pending {
                call.cancel
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            pending
        });
    }

    /// Phase A: with nothing to decide, wait for input, a call's deadline or a final
    /// check's retry tick. True at end of input.
    fn wait_idle(&mut self) -> bool {
        if self.arrivals.is_empty() && self.completed.is_empty() && self.checked.is_empty() {
            let retry = self
                .checks
                .values()
                .filter(|check| !check.running)
                .filter_map(|check| check.retry_at)
                .min();
            let next = self
                .in_flight
                .values()
                .filter(|_| self.workspace.call_timer_enabled())
                .map(|call| call.deadline)
                .chain(retry)
                .min();
            match self.inbox.wait(next) {
                Some(Input::Frame(frame, relay, accepted, _)) => {
                    if receive(
                        frame,
                        relay,
                        accepted,
                        &mut self.session,
                        &mut self.arrivals,
                    ) {
                        self.cancel_dropped();
                        return true;
                    }
                }
                Some(Input::Done(token, outcome)) => self.completed.push_back((token, outcome)),
                Some(Input::FinalChecked(token, result)) => self.checked.push_back((token, result)),
                None if self.inbox.ended => {
                    self.session.eof();
                    self.cancel_dropped();
                    return true;
                }
                None => {}
            }
        }
        false
    }

    /// Phase B: each finished call's blocking final step, before the fence.
    fn settle_completed(&mut self) -> std::io::Result<Vec<Finished>> {
        use std::time::Instant;
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
        while let Some((token, outcome)) = self.completed.pop_front() {
            let Some(call) = self.in_flight.get(&token) else {
                // Already decided at its deadline, or cancelled.
                continue;
            };
            let outcome = outcome?;
            let selected = match &outcome.finals {
                Finals::Selected { witness, .. } if self.workspace.clock_now() < call.deadline => {
                    Some((witness.clone(), "selected_final"))
                }
                Finals::Capacity { witness, .. } if self.workspace.clock_now() < call.deadline => {
                    Some((witness.clone(), "capacity_final"))
                }
                _ => None,
            };
            if let Some((witness, stage)) = selected {
                let running = start_final_check(
                    token,
                    &witness,
                    self.workspace.helper_deadline(call.deadline),
                    stage,
                    &self.sender,
                );
                let retry_at = (!running).then(|| Instant::now() + FINAL_RECHECK_TICK);
                self.checks.insert(
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
            let mut show = outcome.show;
            if self.workspace.clock_now() < call.deadline
                && let Finals::Launch { catching_up } = &outcome.finals
            {
                phase_hook("launch_final")?;
                if self.workspace.check().is_err() {
                    show = None;
                    result = tools::resolved_root_changed(
                        &call.name,
                        &call.id,
                        self.workspace.root(),
                        *catching_up,
                        call.modern,
                    );
                }
            }
            #[cfg(feature = "test-support")]
            if let Some(hook) = &self.workspace.hooks.send_decision {
                hook(&call.id);
            }
            finished.push((token, result, None, show));
        }
        Ok(finished)
    }

    /// Phase C: each final identity check's result.
    fn settle_checked(&mut self, finished: &mut Vec<Finished>) -> std::io::Result<()> {
        use std::time::Instant;
        while let Some((token, result)) = self.checked.pop_front() {
            let result = result?;
            let (Some(mut check), Some(call)) =
                (self.checks.remove(&token), self.in_flight.get(&token))
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
                    if self.workspace.clock_now() < call.deadline {
                        check.running = false;
                        check.retry_at = Some(Instant::now() + FINAL_RECHECK_TICK);
                        self.checks.insert(token, check);
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
                    check.outcome.show = None;
                    None
                }
            };
            #[cfg(feature = "test-support")]
            if let Some(hook) = &self.workspace.hooks.send_decision {
                hook(&call.id);
            }
            finished.push((token, check.outcome.result, gate, check.outcome.show));
        }
        Ok(())
    }

    /// Phase D: forget the checks of calls already decided or expired, and start again
    /// each check that is due.
    fn restart_due_checks(&mut self) {
        use std::time::Instant;
        let now = Instant::now();
        self.checks.retain(|token, _| {
            self.in_flight
                .get(token)
                .is_some_and(|call| self.workspace.clock_now() < call.deadline)
        });
        for (token, check) in &mut self.checks {
            if check.running || check.retry_at.is_some_and(|at| now < at) {
                continue;
            }
            let (Finals::Selected { witness, .. } | Finals::Capacity { witness, .. }) =
                &check.outcome.finals
            else {
                unreachable!("only selected calls have final checks")
            };
            let deadline = self
                .workspace
                .helper_deadline(self.in_flight[token].deadline);
            check.running =
                start_final_check(*token, witness, deadline, "final_recheck", &self.sender);
            check.retry_at = (!check.running).then(|| now + FINAL_RECHECK_TICK);
        }
    }

    /// Phase E: the fence for this turn's decisions. Returns the requests to decide now,
    /// or `None` at end of input.
    fn fence_decisions(
        &mut self,
        finished: &[Finished],
    ) -> Option<std::collections::VecDeque<impl_receive::Outcome>> {
        #[cfg(feature = "test-support")]
        if let Some(hook) = &self.workspace.hooks.immediate_decision {
            for arrival in &self.arrivals {
                if let impl_receive::Outcome::Request(admission, _, _) = arrival
                    && matches!(admission.action, session::Action::Call { .. })
                {
                    hook(&admission.id);
                }
            }
        }
        // The fence for every decision below. Requests it handles are decided
        // after the next one.
        let deciding = std::mem::take(&mut self.arrivals);
        if self.fence() {
            for (token, _, _, _) in finished {
                if let Some(call) = self.in_flight.get(token) {
                    call.cancel
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            }
            self.cancel_dropped();
            return None;
        }
        self.cancel_dropped();
        #[cfg(feature = "test-support")]
        if let Some(hook) = &self.workspace.hooks.fence_handled {
            hook(deciding.len() + self.arrivals.len());
        }
        Some(deciding)
    }

    /// Phase F: the send decision of each finished call.
    fn send_finished(&mut self, finished: Vec<Finished>) -> std::io::Result<()> {
        use std::time::Instant;
        // Each decision: cancelled (not pending) -> deadline -> outcome. A
        // selected call's identity is compared again here, at its send, with
        // filesystem calls only: its helper's success may be stale after
        // whatever the coordinator did since, such as writing to a slow
        // client (#131). A comparison that fails with an I/O error proving
        // nothing (`Unavailable`) leaves the call pending: its final check
        // starts again after a tick, until its deadline.
        for (token, result, gate, show) in finished {
            let Some(call) = self.in_flight.remove(&token) else {
                continue;
            };
            phase_hook("call_send")?;
            // The show recorded with this answer, only if the answer stays this success.
            let mut record = None;
            let result = if self.workspace.clock_now() >= call.deadline {
                deadline_answer(&call)
            } else if let Some(gate) = gate {
                phase_hook("selected_call_send")?;
                match gate.witness.verify_at_send(&gate.dirs) {
                    Ok(()) => {
                        record = show;
                        result
                    }
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
                        self.checks.insert(
                            token,
                            FinalCheck {
                                outcome: CallOutcome {
                                    result,
                                    finals,
                                    show,
                                },
                                running: false,
                                retry_at: Some(Instant::now() + FINAL_RECHECK_TICK),
                            },
                        );
                        self.in_flight.insert(token, call);
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
                record = show;
                result
            };
            // The final send decision: a pending call is committed now, so its show is
            // recorded with it; a cancelled call answers nothing and records nothing.
            let result = match record {
                Some(pending) if self.session.pending(token).is_some() => {
                    show::commit(pending, result, &call.name, call.modern)
                }
                _ => result,
            };
            let response = architecture::response(&call.id, result);
            if let Some(value) = self
                .session
                .prepare(token, response)
                .and_then(|prepared| self.session.commit(prepared))
            {
                #[cfg(feature = "test-support")]
                let value = {
                    let mut value = value;
                    if let Some(hook) = &self.workspace.hooks.committed_response {
                        hook(&call.id, &mut value);
                    }
                    value
                };
                send_call(&mut self.writer, &call, &value)?;
            }
        }
        Ok(())
    }

    /// Phase G: answer each call past its deadline, in token order.
    fn expire(&mut self) -> std::io::Result<()> {
        let now = self.workspace.clock_now();
        let mut expired: Vec<u64> = self
            .in_flight
            .iter()
            .filter(|(_, call)| call.deadline <= now)
            .map(|(token, _)| *token)
            .collect();
        expired.sort_unstable();
        for token in expired {
            let call = self.in_flight.remove(&token).expect("expired call");
            call.cancel
                .store(true, std::sync::atomic::Ordering::Release);
            let response = architecture::response(&call.id, deadline_answer(&call));
            if let Some(value) = self
                .session
                .prepare(token, response)
                .and_then(|prepared| self.session.commit(prepared))
            {
                wire::write_relayed_response(&mut self.writer, call.relay, &value)?;
            }
        }
        Ok(())
    }

    /// Phase H: decide each request handled at the previous fence.
    fn admit(
        &mut self,
        deciding: std::collections::VecDeque<impl_receive::Outcome>,
    ) -> std::io::Result<()> {
        let mut non_calls = 0;
        for arrival in deciding {
            let (admission, accepted, relay) = match arrival {
                impl_receive::Outcome::Error(error, relay) => {
                    // Never admitted, so no cancel can name it.
                    wire::write_relayed_response(&mut self.writer, relay, &error.response())?;
                    continue;
                }
                impl_receive::Outcome::Request(admission, accepted, relay) => {
                    (admission, accepted, relay)
                }
            };
            if self.session.pending(admission.token).is_none() {
                // Cancelled at the fence.
                continue;
            }
            let modern = self.session.mode() == session::Mode::Modern;
            let deadline = accepted + self.workspace.call_deadline;
            let result = match &admission.action {
                session::Action::Unknown => None,
                session::Action::Call { name, arguments } => {
                    let attempted = attempted_workspace(arguments.as_ref(), &self.workspace);
                    let refuse = |code: &str| {
                        Some(tools::result(
                            name,
                            tools::admission_failure(&admission.id, code, &attempted),
                            modern,
                        ))
                    };
                    // #528: the daemon-wide worker limit names itself.
                    let busy = |message: String| {
                        let mut failure = tools::admission_failure(
                            &admission.id,
                            "too_many_requests",
                            &attempted,
                        );
                        failure["error"]["message"] = serde_json::json!(message);
                        Some(tools::result(name, failure, modern))
                    };
                    if self.workspace.clock_now() >= deadline {
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
                                &self.workspace,
                            )
                            .response,
                        )
                    } else if live_calls(&self.session, &self.in_flight) >= MAX_CONNECTION_CALLS {
                        // The message names the per-connection limit (#528).
                        refuse("too_many_requests")
                    } else if let Some(permit) =
                        WorkerPermit::acquire(self.workspace.call_workers())
                    {
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let resolved: Resolved = Default::default();
                        let context = CallContext {
                            workspace: self.workspace.clone(),
                            registry: self.registry.clone(),
                            daemon_handle: self.daemon_handle.clone(),
                            id: admission.id.clone(),
                            modern,
                            name: name.clone(),
                            arguments: arguments.clone(),
                            deadline,
                            cancel: cancel.clone(),
                            resolved: resolved.clone(),
                        };
                        let done = self.sender.clone();
                        let token = admission.token;
                        #[cfg(feature = "test-support")]
                        let worker_done = self.workspace.hooks.worker_done.clone();
                        let scope = crate::testing::inherit_scope();
                        // A read held out by a spilled publication answers typed
                        // `rebuild_in_progress` before the call's deadline (#905).
                        let answer_by = self.workspace.helper_deadline(deadline);
                        let answer_by = answer_by
                            .checked_sub(SPILL_ANSWER_TIME)
                            .unwrap_or(answer_by);
                        let spawned =
                            std::thread::Builder::new()
                                .name("mcp-call".into())
                                .spawn(move || {
                                    let _scope = crate::testing::enter_scope(scope);
                                    let _permit = permit;
                                    let outcome =
                                        crate::store::with_spill_answer_by(answer_by, || {
                                            run_call(context)
                                        });
                                    let _ = done.send(Input::Done(token, outcome));
                                    #[cfg(feature = "test-support")]
                                    if let Some(hook) = worker_done {
                                        hook();
                                    }
                                });
                        match spawned {
                            Ok(_) => {
                                self.in_flight.insert(
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
                            Err(_) => busy(format!(
                                "Too many concurrent tool calls in the daemon (limit {MAX_CALL_WORKERS}); retry shortly"
                            )),
                        }
                    } else {
                        busy(format!(
                            "Too many concurrent tool calls in the daemon (limit {MAX_CALL_WORKERS}); retry shortly"
                        ))
                    }
                }
                action => {
                    non_calls += 1;
                    if non_calls > MAX_CONNECTION_REQUESTS {
                        let response = serde_json::json!({"jsonrpc":"2.0","id":admission.id,
                            "error":{"code":-32000,"message":format!("Too many outstanding requests on this connection (limit {MAX_CONNECTION_REQUESTS})"),
                                "data":{"code":"too_many_requests"}}});
                        if let Some(value) = self
                            .session
                            .prepare(admission.token, response)
                            .and_then(|prepared| self.session.commit(prepared))
                        {
                            wire::write_relayed_response(&mut self.writer, relay, &value)?;
                        }
                        continue;
                    }
                    Some(match action {
                        session::Action::Discover => {
                            catalog::discover(self.workspace.build_version())
                        }
                        session::Action::Initialize => {
                            catalog::initialize(self.workspace.build_version())
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
            if let Some(value) = self
                .session
                .prepare(admission.token, response)
                .and_then(|prepared| self.session.commit(prepared))
            {
                wire::write_relayed_capped(&mut self.writer, relay, &value, cap)?;
            }
        }
        Ok(())
    }
}

/// A call's `deadline_exceeded` answer, in the resolved shape once read admission sampled
/// its root.
fn deadline_answer(call: &InFlight) -> serde_json::Value {
    let envelope = match &*call.resolved.lock().unwrap() {
        Some((root, catching_up)) => tools::attributed_root(
            tools::failure(&call.id, "deadline_exceeded"),
            root,
            *catching_up,
        ),
        None => tools::admission_failure(&call.id, "deadline_exceeded", &call.attempted),
    };
    tools::result(&call.name, envelope, call.modern)
}

/// Writes one call's committed response. A response over the cap answers that call
/// `response_too_large` instead (#930), in the shape its deadline failure would take, so
/// one oversized answer never ends the connection or the other calls on it; only a failed
/// write does.
fn send_call(
    writer: &mut impl std::io::Write,
    call: &InFlight,
    response: &serde_json::Value,
) -> std::io::Result<()> {
    match wire::write_relayed_response(writer, call.relay, response) {
        Err(error) if wire::is_too_large(&error) => {
            let envelope = match &*call.resolved.lock().unwrap() {
                Some((root, catching_up)) => {
                    let mut answer =
                        tools::attributed_root(serde_json::json!({}), root, *catching_up);
                    answer["evidenceBasis"] =
                        response["result"]["structuredContent"]["evidenceBasis"].clone();
                    tools::response_too_large(&call.id, &answer)
                }
                None => {
                    let mut envelope =
                        tools::response_too_large(&call.id, &serde_json::Value::Null);
                    envelope["error"]["attemptedWorkspace"] = tools::attempted(&call.attempted);
                    envelope
                }
            };
            let result = tools::result(&call.name, envelope, call.modern);
            wire::write_relayed_response(
                writer,
                call.relay,
                &architecture::response(&call.id, result),
            )
        }
        written => written,
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
        let codes = catalog["tools"][0]["outputSchema"]["$defs"]["Failure"]["properties"]["error"]
            ["properties"]["code"]["enum"]
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
        let codes = catalog["tools"][0]["outputSchema"]["$defs"]["Failure"]["properties"]["error"]
            ["properties"]["code"]["enum"]
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
