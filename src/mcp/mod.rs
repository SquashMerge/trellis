//! Bounded read-only MCP protocol and single-connection stdio transport.
pub mod catalog;
mod projection;
pub mod session;
pub mod tools;
pub mod wire;

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

/// Test-only pause points on one connection's transport.
#[cfg(feature = "test-support")]
#[doc(hidden)]
#[derive(Clone, Default)]
pub struct TransportHooksForTests {
    /// On the reader thread, after each frame is complete and counted, before
    /// it is published to the coordinator.
    pub frame_read: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// On the coordinator, when a decision waits for a frame completed before
    /// it but not yet published.
    pub settle_waiting: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// On the coordinator, before the final input pass of an immediate call
    /// outcome (validation or capacity); given the call's ID.
    pub immediate_decision: Option<CallHookForTests>,
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

fn mcp_read_error(error: &anyhow::Error) -> &'static str {
    if error
        .downcast_ref::<crate::store::topology::StorageBusy>()
        .is_some()
    {
        return "deadline_exceeded";
    }
    if error
        .downcast_ref::<crate::store::topology::IndexNotReady>()
        .is_some()
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

struct DescribedSnapshot {
    data: Option<(crate::model::IndexStatus, [[u64; 5]; 4])>,
    catching_up: bool,
    state: &'static str,
    progress: Option<serde_json::Value>,
}

/// Every tool call is answered within this deadline; describe reports it as
/// `limits.deadlineMs` (contract W1).
pub const CALL_DEADLINE: std::time::Duration = std::time::Duration::from_millis(5000);

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
                            let status = response.status()?;
                            let counts = response.mcp_language_counts()?;
                            response.finish((status, counts))
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

/// An opt-in, bounded fixture barrier at an actual coordinator boundary.
/// Normal MCP calls do not connect to or wait for a test socket.
#[cfg(feature = "test-support")]
pub(crate) fn phase_hook(stage: &str) -> std::io::Result<()> {
    use std::io::{Read, Write};
    if std::env::var("TRELLIS_TEST_MCP_PHASE").ok().as_deref() != Some(stage) {
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
    relay: Option<u64>,
    modern: bool,
    deadline: std::time::Instant,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    resolved: Resolved,
    attempted: serde_json::Value,
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
            Some(path) => registry
                .blocking_lock()
                .select(
                    *session_id,
                    workspace.identity(),
                    std::path::Path::new(path),
                )
                .map(Some),
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
                        result: tools::resolved_root_changed(&id, root, catching_up, modern),
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
                    Ok((read, degraded)) => {
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
            *resolved.lock().unwrap() = Some((answering.root().to_owned(), sampled_catching_up));
            envelope["workspace"] = serde_json::json!(answering.root());
            envelope["catchingUp"] = serde_json::json!(sampled_catching_up);
            tool.response = tools::result(envelope, modern);
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
            let checked_capacity = {
                let guard = registry
                    .as_ref()
                    .expect("daemon registry required")
                    .0
                    .blocking_lock();
                guard.capacity_witness(workspace.identity(), std::path::Path::new(path))
            };
            match checked_capacity {
                Ok(witness) => {
                    let catching_up = catching_up(&registry, &witness.identity.root_key);
                    *resolved.lock().unwrap() = Some((witness.identity.root.clone(), catching_up));
                    let result = tools::result(
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
                result: tools::result(tools::selection_failure(&id, error, &attempted), modern),
                finals: Finals::None,
            })
        }
    }
}

/// Activation, the selected read and published watcher health for one
/// describe call, on the call's own worker.
fn describe(
    registry: &std::sync::Arc<tokio::sync::Mutex<crate::daemon::registry::CheckoutRegistry>>,
    key: &str,
    selected: Option<&crate::daemon::registry::SelectedCheckout>,
    daemon_handle: Option<&tokio::runtime::Handle>,
    deadline: std::time::Instant,
    cancel: &std::sync::atomic::AtomicBool,
    admitted: &dyn Fn(bool),
) -> anyhow::Result<(anyhow::Result<DescribedSnapshot>, anyhow::Result<bool>)> {
    let existing = registry.blocking_lock().runtime(key);
    let runtime = match existing {
        Some(runtime) => runtime,
        None => match daemon_handle {
            Some(handle) => handle.block_on(async { registry.lock().await.activate(key) })?,
            None => anyhow::bail!("index_not_ready: checkout not activated"),
        },
    };
    let read = read_describe(&runtime, selected, deadline, cancel, admitted);
    Ok((read, runtime.mcp_watcher_degraded()))
}

/// Outstanding non-call requests per connection; more are refused at once
/// (Decision 0009 §7, as for the relay).
const MAX_CONNECTION_REQUESTS: usize = 16;
/// Protocol errors held before they are written within the settling pass.
const MAX_QUEUED_ERRORS: usize = 16;

/// The coordinator's input: worker completions, and frames in input order,
/// each numbered by the reader when complete. A decision settles every frame
/// completed before it, waiting for one the reader counted but has not yet
/// published, and no frame completed after it.
struct Inbox {
    receiver: std::sync::mpsc::Receiver<Input>,
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
    use std::collections::{HashMap, VecDeque};
    use std::io::BufReader;
    use std::time::Instant;
    let workspace = std::sync::Arc::new(workspace);
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Input>(32);
    let reader_sender = sender.clone();
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
                let accepted = Instant::now();
                // Counted when complete, before it is published: a decision
                // that samples the count waits for every frame it covers.
                let seq = counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel) + 1;
                match read {
                    Ok((_, wire::Frame::Eof)) => {
                        let _ =
                            reader_sender.send(Input::Frame(wire::Frame::Eof, None, accepted, seq));
                        break;
                    }
                    Ok((relay, frame)) => {
                        #[cfg(feature = "test-support")]
                        if let Some(hook) = &frame_read {
                            hook();
                        }
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
                        let _ =
                            reader_sender.send(Input::Frame(wire::Frame::Eof, None, accepted, seq));
                        break;
                    }
                }
            }
        })?;
    let mut inbox = Inbox {
        receiver,
        completed,
        received: 0,
        later: None,
        ended: false,
        #[cfg(feature = "test-support")]
        waiting_hook: workspace.hooks.settle_waiting.clone(),
    };
    let mut session = session::Session::new();
    let mut outcomes = VecDeque::new();
    let mut completed: VecDeque<(u64, std::io::Result<CallOutcome>)> = VecDeque::new();
    let mut in_flight: HashMap<u64, InFlight> = HashMap::new();
    let mut writer = output;
    // The decision point: settle every frame completed before now. Returns
    // true at end of input. A request beyond the connection's bounds (16
    // non-call requests, 4 calls counting those running) is refused, and
    // excess protocol errors are written; both are committed only after the
    // pass, so a cancel completed before the decision still wins.
    let settle = |inbox: &mut Inbox,
                  session: &mut session::Session,
                  outcomes: &mut VecDeque<impl_receive::Outcome>,
                  completed: &mut VecDeque<(u64, std::io::Result<CallOutcome>)>,
                  running: usize,
                  writer: &mut dyn std::io::Write|
     -> std::io::Result<bool> {
        let until = inbox.decision();
        let mut refused = Vec::new();
        while let Some(input) = inbox.settled(until) {
            match input {
                Input::Frame(frame, relay, accepted, _) => {
                    if receive(frame, relay, accepted, session, outcomes) {
                        return Ok(true);
                    }
                    if over_bounds(outcomes, running) {
                        refused.push(outcomes.pop_back().expect("just queued"));
                    }
                }
                Input::Done(token, outcome) => completed.push_back((token, outcome)),
            }
        }
        for outcome in refused {
            refuse_queued(outcome, session, &workspace, writer)?;
        }
        if inbox.ended {
            session.eof();
            return Ok(true);
        }
        Ok(false)
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
        tools::result(envelope, call.modern)
    };
    loop {
        if outcomes.is_empty() && completed.is_empty() {
            let next = in_flight.values().map(|call| call.deadline).min();
            match inbox.wait(next) {
                Some(Input::Frame(frame, relay, accepted, _)) => {
                    if receive(frame, relay, accepted, &mut session, &mut outcomes) {
                        cancel_dropped(&session, &mut in_flight);
                        return Ok(());
                    }
                }
                Some(Input::Done(token, outcome)) => completed.push_back((token, outcome)),
                None if inbox.ended => {
                    session.eof();
                    cancel_dropped(&session, &mut in_flight);
                    return Ok(());
                }
                None => {}
            }
        }
        if settle(
            &mut inbox,
            &mut session,
            &mut outcomes,
            &mut completed,
            in_flight.len(),
            &mut writer,
        )? {
            cancel_dropped(&session, &mut in_flight);
            return Ok(());
        }
        cancel_dropped(&session, &mut in_flight);
        // Calls past their deadline answer deadline_exceeded now, each after
        // its own input pass so an earlier cancel still wins.
        let now = Instant::now();
        let mut expired: Vec<u64> = in_flight
            .iter()
            .filter(|(_, call)| call.deadline <= now)
            .map(|(token, _)| *token)
            .collect();
        expired.sort_unstable();
        for token in expired {
            if settle(
                &mut inbox,
                &mut session,
                &mut outcomes,
                &mut completed,
                in_flight.len(),
                &mut writer,
            )? {
                cancel_dropped(&session, &mut in_flight);
                return Ok(());
            }
            cancel_dropped(&session, &mut in_flight);
            let Some(call) = in_flight.remove(&token) else {
                continue;
            };
            call.cancel
                .store(true, std::sync::atomic::Ordering::Release);
            let response = serde_json::json!({"jsonrpc":"2.0","id":call.id,
                "result":deadline_answer(&call)});
            if let Some(prepared) = session.prepare(token, response)
                && let Some(value) = session.commit(prepared)
            {
                wire::write_relayed_response(&mut writer, call.relay, &value)?;
            }
        }
        // Finished calls: the send decision.
        while let Some((token, outcome)) = completed.pop_front() {
            if settle(
                &mut inbox,
                &mut session,
                &mut outcomes,
                &mut completed,
                in_flight.len(),
                &mut writer,
            )? {
                cancel_dropped(&session, &mut in_flight);
                return Ok(());
            }
            cancel_dropped(&session, &mut in_flight);
            let Some(call) = in_flight.remove(&token) else {
                // Already decided at its deadline, or cancelled.
                continue;
            };
            let outcome = outcome?;
            let mut result = outcome.result;
            if Instant::now() < call.deadline {
                match &outcome.finals {
                    Finals::None => {}
                    Finals::Launch { catching_up } => {
                        phase_hook("launch_final")?;
                        if workspace.check().is_err() {
                            result = tools::resolved_root_changed(
                                &call.id,
                                workspace.root(),
                                *catching_up,
                                call.modern,
                            );
                        }
                    }
                    Finals::Selected {
                        witness,
                        catching_up,
                    } => {
                        phase_hook("selected_final")?;
                        if witness.before_answer().is_err() {
                            result = tools::resolved_root_changed(
                                &call.id,
                                &witness.identity.root,
                                *catching_up,
                                call.modern,
                            );
                        }
                    }
                    Finals::Capacity {
                        witness,
                        catching_up,
                    } => {
                        phase_hook("capacity_final")?;
                        if witness.before_answer().is_err() {
                            result = tools::resolved_root_changed(
                                &call.id,
                                &witness.identity.root,
                                *catching_up,
                                call.modern,
                            );
                        }
                    }
                }
            }
            #[cfg(feature = "test-support")]
            if let Some(hook) = &workspace.hooks.send_decision {
                hook(&call.id);
            }
            // The final root check may block: settle input received by now
            // (a cancel wins), then recheck the deadline, then decide.
            if settle(
                &mut inbox,
                &mut session,
                &mut outcomes,
                &mut completed,
                in_flight.len(),
                &mut writer,
            )? {
                call.cancel
                    .store(true, std::sync::atomic::Ordering::Release);
                cancel_dropped(&session, &mut in_flight);
                return Ok(());
            }
            cancel_dropped(&session, &mut in_flight);
            if Instant::now() >= call.deadline {
                result = deadline_answer(&call);
            }
            let response = serde_json::json!({"jsonrpc":"2.0","id":call.id,"result":result});
            if let Some(prepared) = session.prepare(token, response)
                && let Some(value) = session.commit(prepared)
            {
                wire::write_relayed_response(&mut writer, call.relay, &value)?;
            }
        }
        let Some(outcome) = outcomes.pop_front() else {
            continue;
        };
        match outcome {
            impl_receive::Outcome::Error(error, relay) => {
                wire::write_relayed_response(&mut writer, relay, &error.response())?
            }
            impl_receive::Outcome::Request(admission, accepted, relay) => {
                // Cancellation may have invalidated the token while its outcome waited.
                if session.pending(admission.token).is_none() {
                    continue;
                }
                let modern = session.mode() == session::Mode::Modern;
                let deadline = accepted + workspace.call_deadline;
                let immediate = match &admission.action {
                    session::Action::Discover => Some(catalog::discover(workspace.build_version())),
                    session::Action::Initialize => {
                        Some(catalog::initialize(workspace.build_version()))
                    }
                    session::Action::List => Some(catalog::list(modern)),
                    session::Action::Unknown => None,
                    session::Action::Call { name, arguments } => {
                        let attempted = attempted_workspace(arguments.as_ref(), &workspace);
                        if Instant::now() >= deadline {
                            // Accepted, but the deadline passed before admission.
                            Some(tools::result(
                                tools::admission_failure(
                                    &admission.id,
                                    "deadline_exceeded",
                                    &attempted,
                                ),
                                modern,
                            ))
                        } else if tools::validate(name, arguments.as_ref()).is_err() {
                            // Application validation precedes capacity.
                            let tool = tools::prepare(
                                name,
                                arguments.as_ref(),
                                &admission.id,
                                modern,
                                &workspace,
                            );
                            Some(tool.response)
                        } else if in_flight.len() >= MAX_CONNECTION_CALLS {
                            Some(tools::result(
                                tools::admission_failure(
                                    &admission.id,
                                    "too_many_requests",
                                    &attempted,
                                ),
                                modern,
                            ))
                        } else if let Some(permit) = WorkerPermit::acquire() {
                            let cancel =
                                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
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
                            let spawned = std::thread::Builder::new()
                                .name("mcp-call".into())
                                .spawn(move || {
                                    let _permit = permit;
                                    let outcome = run_call(context);
                                    let _ = done.send(Input::Done(token, outcome));
                                });
                            match spawned {
                                Ok(_) => {
                                    in_flight.insert(
                                        token,
                                        InFlight {
                                            id: admission.id.clone(),
                                            relay,
                                            modern,
                                            deadline,
                                            cancel,
                                            resolved,
                                            attempted,
                                        },
                                    );
                                    None
                                }
                                // The permit went with the failed closure.
                                Err(_) => Some(tools::result(
                                    tools::admission_failure(
                                        &admission.id,
                                        "too_many_requests",
                                        &attempted,
                                    ),
                                    modern,
                                )),
                            }
                        } else {
                            Some(tools::result(
                                tools::admission_failure(
                                    &admission.id,
                                    "too_many_requests",
                                    &attempted,
                                ),
                                modern,
                            ))
                        }
                    }
                };
                let response = match (&admission.action, immediate) {
                    (session::Action::Unknown, _) => {
                        wire::ProtocolError::new(-32601, admission.id.clone()).response()
                    }
                    (_, Some(result)) => {
                        serde_json::json!({"jsonrpc":"2.0","id":admission.id,"result":result})
                    }
                    // Dispatched to a worker; decided later.
                    (_, None) => continue,
                };
                let call = matches!(admission.action, session::Action::Call { .. });
                #[cfg(feature = "test-support")]
                if call && let Some(hook) = &workspace.hooks.immediate_decision {
                    hook(&admission.id);
                }
                let Some(prepared) = session.prepare(admission.token, response) else {
                    continue;
                };
                // A final input pass takes precedence over the unsent result.
                if settle(
                    &mut inbox,
                    &mut session,
                    &mut outcomes,
                    &mut completed,
                    in_flight.len(),
                    &mut writer,
                )? {
                    cancel_dropped(&session, &mut in_flight);
                    return Ok(());
                }
                // The deadline overrides an outcome still unsent at it.
                let prepared = if call && Instant::now() >= deadline {
                    let attempted = match &admission.action {
                        session::Action::Call { arguments, .. } => {
                            attempted_workspace(arguments.as_ref(), &workspace)
                        }
                        _ => unreachable!("checked call"),
                    };
                    let expired = serde_json::json!({"jsonrpc":"2.0","id":admission.id,
                    "result":tools::result(
                        tools::admission_failure(&admission.id, "deadline_exceeded", &attempted),
                        modern,
                    )});
                    session.prepare(admission.token, expired)
                } else {
                    Some(prepared)
                };
                if let Some(value) = prepared.and_then(|prepared| session.commit(prepared)) {
                    wire::write_relayed_response(&mut writer, relay, &value)?;
                }
            }
        }
    }
}

/// Whether the outcome just queued is beyond the connection's bounds: more
/// than 16 non-call requests, more than 4 valid calls counting those running,
/// or more than 16 protocol errors. Invalid calls are never counted.
fn over_bounds(
    outcomes: &std::collections::VecDeque<impl_receive::Outcome>,
    running: usize,
) -> bool {
    let valid_call = |outcome: &impl_receive::Outcome| match outcome {
        impl_receive::Outcome::Request(admission, _, _) => match &admission.action {
            session::Action::Call { name, arguments } => {
                tools::validate(name, arguments.as_ref()).is_ok()
            }
            _ => false,
        },
        _ => false,
    };
    let Some(last) = outcomes.back() else {
        return false;
    };
    match last {
        impl_receive::Outcome::Error(..) => {
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, impl_receive::Outcome::Error(..)))
                .count()
                > MAX_QUEUED_ERRORS
        }
        impl_receive::Outcome::Request(admission, _, _) => match admission.action {
            session::Action::Call { .. } => {
                valid_call(last)
                    && running
                        + outcomes
                            .iter()
                            .filter(|outcome| valid_call(outcome))
                            .count()
                        > MAX_CONNECTION_CALLS
            }
            _ => {
                outcomes
                    .iter()
                    .filter(|outcome| {
                        matches!(outcome, impl_receive::Outcome::Request(admission, _, _)
                            if !matches!(admission.action, session::Action::Call { .. }))
                    })
                    .count()
                    > MAX_CONNECTION_REQUESTS
            }
        },
    }
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

/// Refuse an outcome beyond the connection's bounds: a protocol error is
/// written as is; a request gets too_many_requests (a call in the
/// pre-resolution shape). A cancelled request is not answered.
fn refuse_queued(
    outcome: impl_receive::Outcome,
    session: &mut session::Session,
    workspace: &OpenedWorkspace,
    mut writer: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    match outcome {
        impl_receive::Outcome::Error(error, relay) => {
            wire::write_relayed_response(&mut writer, relay, &error.response())
        }
        impl_receive::Outcome::Request(admission, _, relay) => {
            let response = match &admission.action {
                session::Action::Call { arguments, .. } => {
                    let attempted = attempted_workspace(arguments.as_ref(), workspace);
                    let modern = session.mode() == session::Mode::Modern;
                    serde_json::json!({"jsonrpc":"2.0","id":admission.id,
                    "result":tools::result(
                        tools::admission_failure(&admission.id, "too_many_requests", &attempted),
                        modern,
                    )})
                }
                _ => serde_json::json!({"jsonrpc":"2.0","id":admission.id,
                    "error":{"code":-32000,"message":"Too many queued requests",
                        "data":{"code":"too_many_requests"}}}),
            };
            match session
                .prepare(admission.token, response)
                .and_then(|prepared| session.commit(prepared))
            {
                Some(value) => wire::write_relayed_response(&mut writer, relay, &value),
                None => Ok(()),
            }
        }
    }
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
}

#[cfg(test)]
mod settlement_tests {
    use super::*;

    /// Decision 0009 §7: a decision settles exactly the frames completed
    /// before it. A later frame, even one already published, is kept in order
    /// for the next decision.
    #[test]
    fn settlement_takes_only_frames_completed_by_the_decision() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(4);
        let completed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));
        let mut inbox = Inbox {
            receiver,
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
