//! In-memory checkout admission. Retained entries never own checkout resources.
use crate::http;
use crate::http::RuntimePhase;
#[cfg(test)]
use crate::indexer::IndexOptions;
#[cfg(all(test, feature = "test-support"))]
use crate::model::IndexPin;
use crate::model::MAX_ACTIVE_CHECKOUTS;
use crate::producer::auto::LaunchOutcome;
use crate::store::{
    Store,
    topology::{TopologyRoots, WorkspaceIdentity},
};
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub const BROWSER_IDLE_DELAY: Duration = Duration::from_secs(15 * 60);
pub const CHECKOUT_RELEASE_DELAY: Duration = Duration::from_secs(15 * 60);
pub const DAEMON_IDLE_DELAY: Duration = Duration::from_secs(30 * 60);
/// A closing runtime finishes bounded local work (watcher, session, witness
/// handles); activation waits this long for it before reporting not ready.
const RETIRED_WAIT: Duration = Duration::from_secs(5);

// The settled wait reads published state only and installs no hooks, so tests
// built without test-support use it too (#720).
mod settled;
pub use settled::{NotSettled, SettleBusy};

mod lifecycle;
mod probes;
mod registration;
mod runtime_architecture;
mod runtime_cli;
mod runtime_mcp;
mod runtime_search;
mod selection;
#[cfg(feature = "test-support")]
mod test_support;

#[cfg(test)]
use runtime_cli::{cli_index_contention, cli_index_request};

#[derive(Debug, PartialEq, Eq)]
pub struct LifecycleTick {
    pub released: Vec<String>,
    pub exit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    NotAbsolute,
    NotCheckout,
    IdentityChanged,
    Unavailable,
    StorageBusy,
    CheckoutCapacity,
    RegistrationConflict,
}

/// Options are inert data, not a Store, index handle, watcher or leader lease.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckoutOptions(pub serde_json::Value);

/// Values are retained as data. No checkout descriptor is opened by
/// registration; activation uses only its selected checkout's values.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
pub struct BrowserOptions {
    pub browse_root: Option<PathBuf>,
    pub max_file_bytes: u64,
    pub rust_source_roots: Vec<(String, PathBuf)>,
    pub rust_library: Option<PathBuf>,
    pub trusted_rustc: Option<PathBuf>,
    pub cargo_home: Option<PathBuf>,
}
impl Default for BrowserOptions {
    fn default() -> Self {
        Self {
            browse_root: None,
            max_file_bytes: 2_097_152,
            rust_source_roots: Vec::new(),
            rust_library: None,
            trusted_rustc: None,
            cargo_home: None,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct CheckoutMetadata {
    root: PathBuf,
    root_key: String,
    record_id: String,
    device: u64,
    inode: u64,
}
/// A cold checkout's prior-head check, captured under the registry lock and
/// run without it.
pub struct PriorHeadProbe {
    identity: CheckoutMetadata,
    roots: Option<TopologyRoots>,
}
impl CheckoutMetadata {
    fn from_identity(identity: &WorkspaceIdentity) -> Self {
        Self {
            root: identity.root.clone(),
            root_key: identity.root_key.clone(),
            record_id: identity.record_id.clone(),
            device: identity.device,
            inode: identity.inode,
        }
    }
    fn matches(&self, identity: &WorkspaceIdentity) -> bool {
        self.root == identity.root
            && self.root_key == identity.root_key
            && self.record_id == identity.record_id
            && (self.device, self.inode) == (identity.device, identity.inode)
    }
    fn rediscover(&self) -> anyhow::Result<WorkspaceIdentity> {
        let identity = WorkspaceIdentity::discover_unattached(Some(&self.root), &self.root)?
            .attach_existing_marker_readonly()?;
        anyhow::ensure!(
            self.matches(&identity),
            "root_changed: checkout identity changed"
        );
        Ok(identity)
    }
}

/// The recorded root path no longer exists (#1316). Any other answer, such as a
/// replaced root or an unreadable parent, is not proof that it is gone.
fn root_missing(root: &std::path::Path) -> bool {
    std::fs::symlink_metadata(root).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

struct BrowserCheckoutEntry {
    identity: CheckoutMetadata,
    active: bool,
    runtime: Option<Arc<CheckoutRuntime>>,
    replacement_idle: bool,
    lost_reads: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

/// An observational listing. It grants no checkout admission or runtime authority.
pub struct BrowserCheckoutsProbe {
    roots: Option<TopologyRoots>,
    entries: HashMap<String, BrowserCheckoutEntry>,
}

/// Captured registry incarnations; preparation performs no registry mutation.
pub struct SelectionProbe {
    entries: HashMap<String, CheckoutMetadata>,
}
pub struct PreparedSelection {
    witness: SelectedCheckout,
    expected: Option<CheckoutMetadata>,
}

struct Entry {
    identity: CheckoutMetadata,
    registration: Option<CheckoutOptions>,
    sessions: HashSet<u64>,
    released: bool,
    pending_work: bool,
    external_work: bool,
    browser_until: Option<Instant>,
    release_at: Option<Instant>,
    /// This entry's runtime closed because its root was lost: a verified
    /// replacement root may take the path even with stale clients attached.
    root_lost: bool,
    /// The lost root's admission leases, kept after its runtime is dropped:
    /// a read still in flight fences replacement until it finishes.
    lost_reads: Option<Arc<std::sync::atomic::AtomicUsize>>,
}

pub struct CheckoutRegistry {
    entries: HashMap<String, Entry>,
    roots: Option<TopologyRoots>,
    search_fill: Option<Arc<super::search_fill::SearchFill>>,
    search: Option<Arc<super::search::SearchService>>,
    runtimes: HashMap<String, Arc<CheckoutRuntime>>,
    idle_exit_at: Option<Instant>,
    serve_sessions: HashSet<u64>,
    clock_override: Option<Instant>,
    /// Wakes the lifecycle owner: clients came or went, a runtime retired,
    /// accepted work drained or the last lease was released (Decision 0009 §5).
    lifecycle: Arc<tokio::sync::Notify>,
    #[cfg(feature = "test-support")]
    test_after_due_release: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(feature = "test-support")]
    test_next_startup_hook: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(feature = "test-support")]
    test_hold_next_startup_backoff: bool,
    #[cfg(feature = "test-support")]
    test_next_dependency_hook: Option<Arc<dyn Fn(&'static str) + Send + Sync>>,
    #[cfg(feature = "test-support")]
    test_native_watcher: bool,
    #[cfg(feature = "test-support")]
    test_architecture_on_demand: bool,
}

impl Default for CheckoutRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl CheckoutRegistry {
    pub fn new() -> Self {
        Self::starting_at(Instant::now())
    }

    pub fn starting_at(now: Instant) -> Self {
        Self {
            entries: HashMap::new(),
            roots: None,
            search_fill: None,
            search: None,
            runtimes: HashMap::new(),
            idle_exit_at: Some(now + DAEMON_IDLE_DELAY),
            serve_sessions: HashSet::new(),
            clock_override: None,
            lifecycle: Arc::new(tokio::sync::Notify::new()),
            #[cfg(feature = "test-support")]
            test_after_due_release: None,
            #[cfg(feature = "test-support")]
            test_next_startup_hook: None,
            #[cfg(feature = "test-support")]
            test_hold_next_startup_backoff: false,
            #[cfg(feature = "test-support")]
            test_next_dependency_hook: None,
            #[cfg(feature = "test-support")]
            test_native_watcher: false,
            #[cfg(feature = "test-support")]
            test_architecture_on_demand: false,
        }
    }

    /// Supply the daemon-wide fill owner before checkout activation.
    pub fn set_search_fill(&mut self, search_fill: Arc<super::search_fill::SearchFill>) {
        self.search_fill = Some(search_fill);
    }

    pub fn set_search_service(&mut self, search: Arc<super::search::SearchService>) {
        self.search = Some(search);
    }

    /// Supply isolated roots in fixtures. The default uses the user's normal
    /// topology only when a checkout is actually activated.
    pub fn with_roots(roots: TopologyRoots) -> Self {
        Self::with_roots_at(roots, Instant::now())
    }

    pub fn with_roots_at(roots: TopologyRoots, now: Instant) -> Self {
        Self {
            roots: Some(roots),
            ..Self::starting_at(now)
        }
    }

    /// Activate an already attached checkout. Registration and global discovery
    /// never call this method and therefore never open SQLite or acquire locks.
    pub fn activate(&mut self, key: &str) -> anyhow::Result<Arc<CheckoutRuntime>> {
        anyhow::ensure!(
            tokio::runtime::Handle::try_current().is_ok(),
            "checkout activation requires a Tokio runtime"
        );
        let entry = self
            .entries
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("checkout not attached"))?;
        anyhow::ensure!(
            !entry.released && (!entry.sessions.is_empty() || entry.browser_until.is_some()),
            "checkout not attached"
        );
        if let Some(runtime) = self.runtimes.get(key).cloned() {
            let scheduler = runtime.resources()?.scheduler.clone();
            match scheduler.runtime_phase() {
                RuntimePhase::Closing | RuntimePhase::Retired => {
                    // A half-closed runtime is never revived: wait for its
                    // bounded close, then activate a fresh one.
                    anyhow::ensure!(
                        scheduler.wait_retired(RETIRED_WAIT),
                        "index_not_ready: checkout is closing"
                    );
                    self.finish_retired(key);
                }
                // Reattach cancels a requested retirement.
                _ => {
                    drop(scheduler.admission_lease()?);
                    return Ok(runtime);
                }
            }
        }
        let entry = self
            .entries
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("checkout not attached"))?;
        let identity = entry.identity.rediscover()?;
        let roots = match &self.roots {
            Some(roots) => roots.clone(),
            None => TopologyRoots::production()?,
        };
        let store = Store::open(roots, identity)?;
        let initial_indexing = store
            .index_baseline()
            .is_ok_and(|pin| pin.index_revision == 0);
        // Publish only a proved-virgin queue before this runtime is visible to
        // selected status reads. Those reads otherwise compete with the EX
        // stage needed before mandatory startup reconciliation can begin.
        store.prepare_virgin_queue_before_activation()?;
        let config = self.browser_config(&entry.identity.root, entry.registration.as_ref())?;
        // Browser registration remains inert until activation. The scheduler's
        // internal token/address are not bound or exposed by browser provisioning.
        let scheduler = http::new_with_config(
            store.clone(),
            config,
            format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            "127.0.0.1:1".parse()?,
        )?;
        let runtime = Arc::new(CheckoutRuntime {
            resources: std::sync::Mutex::new(Some(Arc::new(ActiveResources {
                store,
                scheduler: scheduler.clone(),
            }))),
            root_lost: std::sync::atomic::AtomicBool::new(false),
            initial_indexing,
            rules_edit_owner: std::sync::Mutex::new(()),
            pickup: crate::producer::pickup::PickupOwner::default(),
            producer: crate::producer::run::ProducerOwner::default(),
            auto_run: crate::producer::auto::AutoRunOwner::default(),
            view_requests: Default::default(),
            search: self.search.clone(),
            search_coverage: std::sync::Mutex::new(None),
            #[cfg(feature = "test-support")]
            mcp_clock_skew_ms: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "test-support")]
            search_hash_scans: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(feature = "test-support")]
            search_coverage_scans: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(feature = "test-support")]
            browser_status_projections: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(feature = "test-support")]
            mcp_hint_clock_origin: std::sync::Mutex::new(None),
            #[cfg(feature = "test-support")]
            mcp_call_clock_origin: std::sync::Mutex::new(None),
            #[cfg(feature = "test-support")]
            mcp_read_hook: std::sync::Mutex::new(None),
            #[cfg(feature = "test-support")]
            mcp_read_gate: std::sync::Mutex::new(None),
            #[cfg(feature = "test-support")]
            mcp_admitted_gate: std::sync::Mutex::new(None),
            #[cfg(feature = "test-support")]
            evidence_sampled_hook: std::sync::Mutex::new(None),
        });
        #[cfg(feature = "test-support")]
        if let Some(reporter) = super::causal_witness::CausalWitness::from_env(key.to_owned()) {
            runtime
                .resources()?
                .scheduler
                .set_causal_witness_runtime(reporter, Arc::downgrade(&runtime));
        }
        self.runtimes.insert(key.to_owned(), runtime.clone());
        let lifecycle = self.lifecycle.clone();
        scheduler.set_lifecycle_wake(Arc::new(move || lifecycle.notify_one()));
        // Installed before startup begins, so the worker cannot pass it first.
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.test_next_startup_hook.take() {
            scheduler.set_startup_hook_for_tests(hook);
        }
        #[cfg(feature = "test-support")]
        if std::mem::take(&mut self.test_hold_next_startup_backoff) {
            scheduler.hold_startup_backoff_for_tests();
        }
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.test_next_dependency_hook.take() {
            scheduler.set_dependency_hook_for_tests(hook);
        }
        #[cfg(feature = "test-support")]
        if self.test_native_watcher {
            scheduler.enable_native_watcher_for_tests();
        }
        #[cfg(feature = "test-support")]
        if self.test_architecture_on_demand {
            scheduler.set_architecture_on_demand_for_tests();
        }
        if let Some(search_fill) = &self.search_fill {
            scheduler.set_search_fill(search_fill.register(&scheduler));
        }
        runtime
            .pickup
            .start_watch(&runtime.resources()?.store, scheduler.clone());
        runtime.auto_run.start(
            Arc::new(AutoHost {
                runtime: Arc::downgrade(&runtime),
            }),
            Arc::new(crate::producer::auto::StoreSources::new(
                runtime.resources()?.store.clone(),
                crate::producer::profiles::Stub::from_env(),
            )),
        );
        scheduler.begin_startup_reconciliation();
        Ok(runtime)
    }

    /// An active runtime. One that is closing or retired is not returned: the
    /// caller activates a fresh one.
    pub fn runtime(&self, key: &str) -> Option<Arc<CheckoutRuntime>> {
        self.runtimes.get(key).cloned().filter(|runtime| {
            runtime.resources().is_ok_and(|resources| {
                !matches!(
                    resources.scheduler.runtime_phase(),
                    RuntimePhase::Closing | RuntimePhase::Retired
                )
            })
        })
    }

    /// The lifecycle owner waits on this between deadlines.
    pub fn lifecycle_signal(&self) -> Arc<tokio::sync::Notify> {
        self.lifecycle.clone()
    }

    /// Wait for a runtime whose retirement was requested to finish closing.
    fn wait_runtime_retired(&self, key: &str) -> bool {
        self.runtimes.get(key).is_some_and(|runtime| {
            runtime.resources().is_ok_and(|resources| {
                resources.scheduler.runtime_phase() != RuntimePhase::Ready
                    && resources.scheduler.wait_retired(RETIRED_WAIT)
            })
        })
    }

    /// Drop a retired runtime's handles and forget it.
    fn finish_retired(&mut self, key: &str) {
        let reads = self.runtimes.get(key).and_then(|runtime| {
            runtime
                .resources()
                .ok()
                .map(|resources| resources.scheduler.lease_count())
        });
        if self.root_lost_runtime(key)
            && let Some(entry) = self.entries.get_mut(key)
        {
            entry.root_lost = true;
            // Every path that drops a root-lost runtime keeps its read fence.
            if reads.is_some() {
                entry.lost_reads = reads;
            }
        }
        if let Some(runtime) = self.runtimes.remove(key) {
            if runtime
                .resources()
                .is_ok_and(|resources| resources.scheduler.checkout_root_loss_retired())
            {
                runtime
                    .root_lost
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            runtime.auto_run.shutdown();
            runtime.pickup.shutdown();
            runtime.resources.lock().unwrap().take();
        }
    }

    pub fn known_roots(&self) -> Vec<(String, PathBuf)> {
        let mut roots: Vec<_> = self
            .entries
            .iter()
            .map(|(key, entry)| (key.clone(), entry.identity.root.clone()))
            .collect();
        roots.sort();
        roots
    }

    pub fn active_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| !entry.released || entry.pending_work)
            .count()
    }

    /// Retire a stale path incarnation only after its owner and accepted work
    /// are quiescent. Never transfer a prior head permit to another identity.
    fn retire_replaced(&mut self, identity: &WorkspaceIdentity) -> Result<(), SelectionError> {
        let key = &identity.root_key;
        let Some(entry) = self.entries.get(key) else {
            return Ok(());
        };
        if entry.identity.matches(identity) {
            return Ok(());
        }
        // A runtime closed for root loss (Decision 0009 §2) has settled the old
        // root's requests. Its stale clients cannot use it (they get
        // root_changed), so they do not keep the replacement from attaching;
        // the new entry starts without them.
        if self.root_lost_runtime(key) {
            // A read still in flight holds the old root's index files, which
            // the replacement would recreate: it fences replacement here as it
            // fences release.
            let reading = self.runtimes.get(key).is_some_and(|runtime| {
                runtime
                    .resources()
                    .is_ok_and(|resources| resources.scheduler.leases() != 0)
            }) || self.entries.get(key).is_some_and(|entry| {
                entry
                    .lost_reads
                    .as_ref()
                    .is_some_and(|reads| reads.load(std::sync::atomic::Ordering::Acquire) != 0)
            });
            if reading
                || !self.verified_replacement(identity)
                || (self.runtimes.contains_key(key) && !self.wait_runtime_retired(key))
            {
                return Err(SelectionError::IdentityChanged);
            }
            self.finish_retired(key);
            self.entries.remove(key);
            return Ok(());
        }
        if !self.can_retire_replaced(identity) {
            return Err(SelectionError::IdentityChanged);
        }
        // The new root's caller is waiting: let the worker finish its bounded
        // close of the replaced runtime now.
        if !self.release(key)? && !(self.wait_runtime_retired(key) && self.release(key)?) {
            return Err(SelectionError::IdentityChanged);
        }
        self.entries.remove(key);
        Ok(())
    }

    /// The checkout's runtime closed (or is closing) because its root was lost.
    fn root_lost_runtime(&self, key: &str) -> bool {
        self.entries.get(key).is_some_and(|entry| entry.root_lost)
            || self.runtimes.get(key).is_some_and(|runtime| {
                runtime
                    .resources()
                    .is_ok_and(|resources| resources.scheduler.checkout_root_loss_retired())
            })
    }

    fn can_retire_replaced(&self, identity: &WorkspaceIdentity) -> bool {
        let Some(entry) = self.entries.get(&identity.root_key) else {
            return true;
        };
        if entry.identity.matches(identity) {
            return true;
        }
        if !entry.sessions.is_empty()
            || entry.browser_until.is_some()
            || entry.pending_work
            || entry.external_work
        {
            return false;
        }
        self.verified_replacement(identity)
    }

    /// Verify a *new* identity independently: rediscovering old metadata
    /// deliberately rejects the changed record_id after `git init`.
    fn verified_replacement(&self, identity: &WorkspaceIdentity) -> bool {
        let Some(entry) = self.entries.get(&identity.root_key) else {
            return true;
        };
        WorkspaceIdentity::discover_unattached(Some(&identity.root), &identity.root)
            .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
            .is_ok_and(|found| {
                !entry.identity.matches(&found)
                    && found.record_id == identity.record_id
                    && (found.device, found.inode) == (identity.device, identity.inode)
            })
    }

    fn attach(
        &mut self,
        session: Option<u64>,
        identity: &WorkspaceIdentity,
    ) -> Result<Arc<WorkspaceIdentity>, SelectionError> {
        self.retire_replaced(identity)?;
        let at_capacity = self.active_count() >= MAX_ACTIVE_CHECKOUTS;
        if let Some(entry) = self.entries.get_mut(&identity.root_key) {
            if !entry.identity.matches(identity) {
                return Err(SelectionError::IdentityChanged);
            }
            if entry.released && at_capacity {
                return Err(SelectionError::CheckoutCapacity);
            }
            let checked = Arc::new(
                identity
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            );
            if let Some(session) = session {
                entry.sessions.insert(session);
            }
            entry.release_at = None;
            entry.released = false;
            self.idle_exit_at = None;
            self.lifecycle.notify_one();
            return Ok(checked);
        }
        if at_capacity {
            return Err(SelectionError::CheckoutCapacity);
        }
        let identity = Arc::new(
            identity
                .verified_clone()
                .map_err(|_| SelectionError::IdentityChanged)?,
        );
        self.entries.insert(
            identity.root_key.clone(),
            Entry {
                identity: CheckoutMetadata::from_identity(&identity),
                registration: None,
                sessions: session.into_iter().collect(),
                released: false,
                pending_work: false,
                external_work: false,
                browser_until: None,
                release_at: None,
                root_lost: false,
                lost_reads: None,
            },
        );
        self.idle_exit_at = None;
        Ok(identity)
    }
}

/// A selected result must pass this fence immediately before its answer is sent.
/// Keeping the captured launch identity prevents later calls from silently
/// treating a changed repository as the original launch repository.
#[derive(Debug, Clone)]
pub struct SelectedCheckout {
    pub identity: Arc<WorkspaceIdentity>,
    launch: Arc<WorkspaceIdentity>,
    launch_common: (u64, u64),
    selected_common: (u64, u64),
}
/// Common Git directories a final check discovered, for its send-time part.
#[derive(Debug, Clone)]
pub struct CommonDirs {
    launch: PathBuf,
    selected: PathBuf,
}
impl std::ops::Deref for SelectedCheckout {
    type Target = WorkspaceIdentity;
    fn deref(&self) -> &Self::Target {
        &self.identity
    }
}

/// Keeps a selected immutable snapshot alive through final workspace and
/// revision checks; idle release cannot drop its owner before this guard drops.
/// Ordering seams for one metrics read and its checked response materialization.
pub struct MetricsResponseHooks<'a> {
    pub before_work: &'a mut dyn FnMut(),
    pub before_finish: &'a mut dyn FnMut(),
    pub after_finish: &'a mut dyn FnMut(),
    pub on_write: &'a mut dyn FnMut(usize),
    /// Called with every progress check, from the request thread or a metrics worker.
    pub on_progress: &'a mut (dyn FnMut() + Send),
}

pub struct CheckoutEvidence {
    response: crate::store::EvidenceResponse,
    /// Held against release until this evidence drops.
    _lease: http::AdmissionLease,
    selection: Option<SelectedCheckout>,
}
impl CheckoutEvidence {
    pub fn finish<T>(&self, value: T) -> anyhow::Result<T> {
        if let Some(selected) = &self.selection {
            selected
                .before_answer()
                .map_err(|e| anyhow::anyhow!("{}", e.reason()))?;
        }
        let result = self.response.finish(value)?;
        if let Some(selected) = &self.selection {
            selected
                .before_answer()
                .map_err(|e| anyhow::anyhow!("{}", e.reason()))?;
        }
        Ok(result)
    }
}
impl std::ops::Deref for CheckoutEvidence {
    type Target = crate::store::EvidenceResponse;
    fn deref(&self) -> &Self::Target {
        &self.response
    }
}

/// The producer layer's view of this runtime (`producer::import::ImportScheduler`): each
/// method is the inherent one of the same name, with the phase reduced to what producers ask.
impl crate::producer::import::ImportScheduler for http::DaemonState {
    fn phase(&self) -> crate::producer::import::SchedulerPhase {
        use crate::producer::import::SchedulerPhase;
        match self.runtime_phase() {
            RuntimePhase::Activating | RuntimePhase::Reconciling { .. } => SchedulerPhase::Starting,
            RuntimePhase::Ready => SchedulerPhase::Ready,
            RuntimePhase::RetireRequested => SchedulerPhase::RetireRequested,
            RuntimePhase::Closing | RuntimePhase::Retired => SchedulerPhase::Stopped,
        }
    }

    fn state_version(&self) -> u64 {
        http::DaemonState::state_version(self)
    }

    fn wait_state_change(&self, seen: u64, timeout: Duration) {
        http::DaemonState::wait_state_change(self, seen, timeout)
    }

    fn enqueue_generated_semantic_import(
        &self,
        options: &crate::store::requests::GeneratedImportOptions,
        id: uuid::Uuid,
    ) -> anyhow::Result<crate::store::requests::Request> {
        http::DaemonState::enqueue_generated_semantic_import(self, options, id)
    }
}

/// Resources are removed from the slot on release, even if a client retains
/// the inert CheckoutRuntime handle after disconnect.
struct ActiveResources {
    store: Store,
    scheduler: Arc<http::DaemonState>,
}

/// One activated checkout has its own worker and no process-global leader.
/// Its runtime state lives with the worker (Decision 0009 §2); a failed
/// startup reconciliation retains no claim authority and is retried.
pub struct CheckoutRuntime {
    /// Test-support virtual time added to this checkout's MCP call clock, so
    /// a test can pass a call deadline without waiting real seconds.
    #[cfg(feature = "test-support")]
    mcp_clock_skew_ms: std::sync::atomic::AtomicU64,
    /// Optional pending-head hash scans that MCP search started (#777).
    #[cfg(feature = "test-support")]
    search_hash_scans: std::sync::atomic::AtomicUsize,
    /// Pending-head cache scans that describe/status coverage started (#806).
    #[cfg(feature = "test-support")]
    search_coverage_scans: std::sync::atomic::AtomicUsize,
    /// Status semantic projections the browser's own status reads ran on this
    /// checkout (#666, #700); per checkout, so no other test's reads count (#869).
    #[cfg(feature = "test-support")]
    browser_status_projections: std::sync::atomic::AtomicUsize,
    #[cfg(feature = "test-support")]
    mcp_call_clock_origin: std::sync::Mutex<Option<Instant>>,
    /// Optional hint clock can be frozen separately from the real mandatory deadline.
    #[cfg(feature = "test-support")]
    mcp_hint_clock_origin: std::sync::Mutex<Option<Instant>>,
    /// Test-support hold point at the start of the next MCP selected read.
    #[cfg(feature = "test-support")]
    mcp_read_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Test-support gate run at the start of every MCP selected read.
    #[cfg(feature = "test-support")]
    mcp_read_gate: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Test-support gate run once read admission has sampled `catchingUp`.
    #[cfg(feature = "test-support")]
    mcp_admitted_gate: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// Test-support hold, once, in the next evidence admission after it sampled
    /// `catchingUp` and before it opens the selected snapshot (#752).
    #[cfg(feature = "test-support")]
    evidence_sampled_hook: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    resources: std::sync::Mutex<Option<Arc<ActiveResources>>>,
    search: Option<Arc<super::search::SearchService>>,
    /// The last head coverage describe/status measured without a matrix (#806).
    search_coverage: std::sync::Mutex<Option<CoverageMemo>>,
    /// Released because its root was lost: a stale handle answers root_changed.
    root_lost: std::sync::atomic::AtomicBool,
    initial_indexing: bool,
    rules_edit_owner: std::sync::Mutex<()>,
    /// The only writer of this checkout's `pickup.json`.
    pickup: crate::producer::pickup::PickupOwner,
    /// The only owner of this checkout's producer runs.
    producer: crate::producer::run::ProducerOwner,
    /// The only owner of this checkout's automatic producer re-runs (#660).
    auto_run: crate::producer::auto::AutoRunOwner,
    /// The latest agent `trellis_show` and whether a viewer is open (#818).
    view_requests: super::view_request::ViewRequests,
}

/// Head coverage measured for one admitted pin while its matrix was not
/// resident (#806), so describe and CLI status do not rescan the head on
/// every call. A pin's declaration count is immutable. The embedded count is
/// keyed by the cache epoch and vector-write generation sampled before its
/// scan, so any later cache write or replacement makes the next call measure
/// again.
struct CoverageMemo {
    pin: crate::model::IndexPin,
    declarations: u64,
    /// `(epoch, writes, embedded)`.
    embedded: Option<(u64, u64, u64)>,
}

/// Startup reconciliation is due or running (not waiting out a backoff).
fn startup_running(phase: RuntimePhase) -> bool {
    matches!(
        phase,
        RuntimePhase::Activating | RuntimePhase::Reconciling { waiting: false, .. }
    )
}

/// A failed startup reconciliation is waiting out its backoff.
fn startup_waiting(phase: RuntimePhase) -> bool {
    matches!(phase, RuntimePhase::Reconciling { waiting: true, .. })
}

/// Tells the closing watcher its run has ended.
struct RunWatchGuard(Arc<std::sync::atomic::AtomicBool>);

impl Drop for RunWatchGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// The checkout runtime as its `AutoRunOwner` sees it (#660). It holds the runtime weakly, so the
/// owner's thread never keeps a retired checkout alive; once the runtime or its resources are
/// gone it reports a runtime that is neither alive nor ready, and the owner exits.
struct AutoHost {
    runtime: std::sync::Weak<CheckoutRuntime>,
}

impl crate::producer::auto::AutoRunRuntime for AutoHost {
    fn alive(&self) -> bool {
        self.runtime
            .upgrade()
            .is_some_and(|runtime| runtime.resources().is_ok())
    }

    fn phase_ready(&self) -> bool {
        self.runtime.upgrade().is_some_and(|runtime| {
            runtime
                .resources()
                .is_ok_and(|resources| resources.scheduler.runtime_phase() == RuntimePhase::Ready)
        })
    }

    fn intent_pending(&self) -> bool {
        let Some(Ok(resources)) = self.runtime.upgrade().map(|runtime| runtime.resources()) else {
            return true;
        };
        // One coherent read (#1082). A checkout with no serving session has its
        // reconciliation and the next watcher's first inventory ahead of it.
        !resources.scheduler.ready_and_quiet()
    }

    fn active_origin(&self) -> Option<crate::producer::state::RunOrigin> {
        match self.runtime.upgrade() {
            Some(runtime) => runtime.producer.active_origin(),
            None => Some(crate::producer::state::RunOrigin::Manual),
        }
    }

    fn supersede(
        &self,
        token: &crate::producer::run::SupersedeToken,
    ) -> crate::producer::run::AutoCancel {
        match self.runtime.upgrade() {
            Some(runtime) => runtime.producer.supersede(token),
            None => crate::producer::run::AutoCancel::NoAutoRun,
        }
    }

    fn state_version(&self) -> u64 {
        self.runtime
            .upgrade()
            .map_or(0, |runtime| runtime.state_version())
    }

    fn wait_state_change(&self, seen: u64, timeout: Duration) {
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.wait_state_change(seen, timeout);
        }
    }

    fn wake(&self) {
        if let Some(Ok(resources)) = self.runtime.upgrade().map(|runtime| runtime.resources()) {
            resources.scheduler.note_state_change();
        }
    }

    fn launch(&self, request: crate::producer::auto::LaunchRequest) -> LaunchOutcome {
        match self.runtime.upgrade() {
            Some(runtime) => runtime.auto_launch(request),
            None => LaunchOutcome::NotReady,
        }
    }
}

impl CheckoutRuntime {
    fn resources(&self) -> anyhow::Result<Arc<ActiveResources>> {
        self.resources
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                if self.root_lost.load(std::sync::atomic::Ordering::Acquire) {
                    anyhow::anyhow!("root_changed: captured root lost")
                } else {
                    anyhow::anyhow!("index_not_ready: checkout released")
                }
            })
    }

    pub fn browser_scheduler(&self) -> anyhow::Result<Arc<http::DaemonState>> {
        Ok(self.resources()?.scheduler.clone())
    }

    /// This checkout's latest agent view request and viewer presence (#818).
    pub fn view_requests(&self) -> &super::view_request::ViewRequests {
        &self.view_requests
    }

    pub(crate) fn search_service(&self) -> Option<&Arc<super::search::SearchService>> {
        self.search.as_ref()
    }

    /// Run one producer after the caller inspected its plan, and reply with the terminal
    /// record. The run holds an admission lease, so the checkout cannot idle-retire under
    /// it; losing the root or closing cancels it as `root_changed`. A client that
    /// disconnects does not cancel the run.
    pub fn cli_producer_run(&self, args: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
        let request = crate::producer::run::RunRequest::parse(args)?;
        let resources = self.resources()?;
        let _lease = resources.scheduler.admission_lease()?;
        resources.store.verify_root()?;
        let stub = crate::producer::profiles::Stub::from_env();
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _finished = RunWatchGuard(done.clone());
        self.watch_run_closing(resources.scheduler.clone(), done);
        let reply = self.producer.start_run(
            &crate::producer::run::RunContext {
                store: &resources.store,
                scheduler: &*resources.scheduler,
                stub: stub.as_ref(),
            },
            &request,
        );
        // The end of a manual run frees the slot an automatic run may be waiting for.
        resources.scheduler.note_state_change();
        reply
    }

    /// Run the approved automatic re-run `request` for this checkout's `AutoRunOwner`, and map
    /// how it ended. The lease is the background kind: it never cancels a retirement, and it is
    /// held only while the run is. A run is admitted only while the checkout is `Ready` with
    /// no FIFO work or watcher intent pending; intent accepted after admission is the worker's
    /// to publish, and a head it moves supersedes the run.
    fn auto_launch(&self, request: crate::producer::auto::LaunchRequest) -> LaunchOutcome {
        use crate::producer::approval::{self, Refusal};
        use crate::producer::auto::{launch_error, run_ended};
        let Ok(resources) = self.resources() else {
            return LaunchOutcome::NotReady;
        };
        let Some(_lease) = resources.scheduler.background_lease() else {
            return LaunchOutcome::NotReady;
        };
        // The owning transition (#1082). The decider's sample of the phase and of pending
        // intent is older than this call: the serving session may have been replaced since,
        // or a hint accepted. The run is admitted on the runtime's state now, under its
        // lock, and a refusal is retried at the decider's next state-signal wake.
        if !resources.scheduler.ready_and_quiet() || resources.store.verify_root().is_err() {
            return LaunchOutcome::NotReady;
        }
        let expected = request.expected_revision.clone();
        let stub = crate::producer::profiles::Stub::from_env();
        let run = match approval::run_request(
            &resources.store,
            &request.approval,
            stub.as_ref(),
            &expected,
            request.token,
        ) {
            Ok(run) => run,
            Err(Refusal::HeadMoved) => return LaunchOutcome::HeadMoved,
            Err(Refusal::Template) => return LaunchOutcome::NeedsApproval,
            Err(Refusal::Failed(code)) => return launch_error(&code, &expected),
        };
        let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let _finished = RunWatchGuard(done.clone());
        self.watch_run_closing(resources.scheduler.clone(), done);
        let result = self.producer.start_run(
            &crate::producer::run::RunContext {
                store: &resources.store,
                scheduler: &*resources.scheduler,
                stub: stub.as_ref(),
            },
            &run,
        );
        match result {
            Err(error) => {
                let message = error.to_string();
                match message.split(':').next().unwrap_or_default() {
                    crate::producer::PLAN_CHANGED => LaunchOutcome::HeadMoved,
                    crate::producer::PRODUCER_BUSY => LaunchOutcome::SlotBusy,
                    "root_changed" => LaunchOutcome::NotReady,
                    _ => launch_error(&message, &expected),
                }
            }
            Ok(reply) => run_ended(&reply, &expected),
        }
    }

    /// Cancel the active run once the runtime closes. Woken by the runtime's own state
    /// change; the wait timeout only bounds how long this thread outlives its run.
    fn watch_run_closing(
        &self,
        scheduler: Arc<http::DaemonState>,
        done: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let owner = self.producer.clone();
        std::thread::spawn(move || {
            while !done.load(std::sync::atomic::Ordering::Acquire) {
                let seen = scheduler.state_version();
                if matches!(
                    scheduler.runtime_phase(),
                    RuntimePhase::Closing | RuntimePhase::Retired
                ) {
                    owner.shutdown();
                    return;
                }
                scheduler.wait_state_change(seen, Duration::from_millis(250));
            }
        });
    }

    /// A retained runtime handle cannot keep the released Store or watcher alive.
    pub fn has_active_resources(&self) -> bool {
        self.resources.lock().unwrap().is_some()
    }

    /// The typed `recovery_blocked: <reason>` of a startup recovery that hit
    /// its failure limit, as `cli_status` reports it (#768).
    pub fn recovery_blocker(&self) -> Option<String> {
        self.resources()
            .ok()
            .and_then(|resources| resources.scheduler.recovery_blocker())
    }

    /// The first error of the current startup reconciliation run.
    pub fn reconciliation_error(&self) -> Option<String> {
        self.resources()
            .ok()
            .and_then(|resources| resources.scheduler.startup_error())
    }

    /// The runtime state version a reader records before retrying a read.
    pub fn state_version(&self) -> u64 {
        self.resources()
            .map_or(0, |resources| resources.scheduler.state_version())
    }

    /// Wait for the runtime state to change after `seen` (startup
    /// reconciliation progressing, for example), at most `timeout`.
    pub fn wait_state_change(&self, seen: u64, timeout: Duration) {
        if let Ok(resources) = self.resources() {
            resources.scheduler.wait_state_change(seen, timeout);
        }
    }

    #[doc(hidden)]
    pub fn retry_waiting_for_tests(&self) -> bool {
        self.resources()
            .is_ok_and(|resources| startup_waiting(resources.scheduler.runtime_phase()))
    }

    /// Advisory status only (Decision 0009 §3); never an admission decision.
    fn catching_up_in(resources: &ActiveResources, phase: RuntimePhase) -> bool {
        match phase {
            RuntimePhase::Ready => {
                resources.scheduler.checkout_watch_pending()
                    || resources.scheduler.fifo_pending()
                    // A publication holding readers out is work in progress; asking the index
                    // would only wait for its COMMIT (#905).
                    || resources.store.publication_holds_readers()
                    || resources
                        .store
                        .pending_semantic_target()
                        .map_or(true, |target| target.is_some())
            }
            _ => true,
        }
    }

    pub fn catching_up(&self) -> bool {
        let Ok(resources) = self.resources() else {
            return true;
        };
        let phase = resources.scheduler.runtime_phase();
        Self::catching_up_in(&resources, phase)
    }

    /// Sample freshness and admit the selected prior head under one runtime gate.
    /// A validated prior head admits reads, never durable mutations or work
    /// scheduling. Only this runtime's committed daemon-local startup reconciliation grants that
    /// authority; the advisory catchingUp flag is not an admission gate.
    pub(crate) fn require_mutation_ready(&self) -> anyhow::Result<()> {
        let resources = self.resources()?;
        anyhow::ensure!(
            resources.scheduler.runtime_phase() != RuntimePhase::Retired,
            "index_not_ready: checkout released"
        );
        anyhow::ensure!(
            !resources.store.root_path_replaced()?,
            "root_changed: captured root lost"
        );
        let Some(session) = resources.scheduler.checkout_serving_owner() else {
            return Err(crate::store::topology::IndexNotReady::new(
                "startup reconciliation not committed",
            )
            .into());
        };
        resources.store.verify_committed_writer_claim(&session)
    }

    pub fn evidence_response(&self) -> anyhow::Result<(CheckoutEvidence, bool)> {
        let resources = self.resources()?;
        let lease = resources.scheduler.admission_lease()?;
        let catching_up = Self::catching_up_in(&resources, resources.scheduler.runtime_phase());
        #[cfg(feature = "test-support")]
        {
            let hook = self.evidence_sampled_hook.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        let response = resources
            .store
            .evidence_response_daemon_local()?
            .with_auto_run_labels(self.auto_run.labels());
        let catching_up = catching_up || response.semantic_catching_up()?;
        Ok((
            CheckoutEvidence {
                response,
                _lease: lease,
                selection: None,
            },
            catching_up,
        ))
    }

    pub fn evidence_for_selected(
        &self,
        selected: &SelectedCheckout,
    ) -> anyhow::Result<(CheckoutEvidence, bool)> {
        selected
            .before_answer()
            .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
        let (mut response, catching_up) = self.evidence_response()?;
        selected
            .before_answer()
            .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
        response.selection = Some(selected.clone());
        Ok((response, catching_up))
    }
}

/// Probe membership before writing a worktree marker. Git output is bounded and
/// the root identity is checked on both sides of the probe.
fn common_identity_unattached(identity: &WorkspaceIdentity) -> Result<(u64, u64), SelectionError> {
    common_identity_unattached_with_executable(identity, &crate::testing::git_executable())
}

fn common_identity_unattached_with_executable(
    identity: &WorkspaceIdentity,
    executable: &Path,
) -> Result<(u64, u64), SelectionError> {
    let root = &identity.root;
    let before = fs::symlink_metadata(root).map_err(|_| SelectionError::Unavailable)?;
    if !before.is_dir()
        || before.file_type().is_symlink()
        || (before.dev(), before.ino()) != (identity.device, identity.inode)
    {
        return Err(SelectionError::IdentityChanged);
    }
    let mut child = std::process::Command::new(executable)
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|_| SelectionError::Unavailable)?;
    let stdout = child.stdout.take().ok_or(SelectionError::Unavailable)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut output = Vec::new();
        let result = stdout.take(4097).read_to_end(&mut output);
        let _ = sender.send(result.map(|_| output));
    });
    let deadline = Instant::now() + Duration::from_millis(750);
    let output = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(Ok(output)) if output.len() <= 4096 => output,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            // A descendant may keep stdout open after Git exits. Never join
            // its reader past the same lookup deadline.
            // At this scale, the detached reader may live until descendant stdout closes.
            drop(reader);
            return Err(SelectionError::Unavailable);
        }
    };
    reader.join().map_err(|_| SelectionError::Unavailable)?;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(
                    Duration::from_millis(5)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SelectionError::Unavailable);
            }
        }
    };
    if !status.success() {
        return Err(SelectionError::Unavailable);
    }
    let path = std::str::from_utf8(&output)
        .map_err(|_| SelectionError::Unavailable)?
        .trim_end_matches('\n');
    if path.contains(['\r', '\n', '\0']) || !Path::new(path).is_absolute() {
        return Err(SelectionError::Unavailable);
    }
    let path = fs::canonicalize(path).map_err(|_| SelectionError::Unavailable)?;
    let after = fs::symlink_metadata(root).map_err(|_| SelectionError::IdentityChanged)?;
    if !after.is_dir()
        || after.file_type().is_symlink()
        || (after.dev(), after.ino()) != (identity.device, identity.inode)
    {
        return Err(SelectionError::IdentityChanged);
    }
    let common = fs::metadata(path).map_err(|_| SelectionError::Unavailable)?;
    if !common.is_dir() {
        return Err(SelectionError::Unavailable);
    }
    Ok((common.dev(), common.ino()))
}

#[cfg(test)]
mod selection_tests;
