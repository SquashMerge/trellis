//! In-memory checkout admission. Retained entries never own checkout resources.
use crate::http::RuntimePhase;
use crate::store::{
    Store,
    topology::{TopologyRoots, WorkspaceIdentity},
};
use crate::{http, indexer::IndexOptions};
use std::{
    collections::{HashMap, HashSet},
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

pub const MAX_ACTIVE_CHECKOUTS: usize = 64;
pub const BROWSER_IDLE_DELAY: Duration = Duration::from_secs(15 * 60);
pub const CHECKOUT_RELEASE_DELAY: Duration = Duration::from_secs(15 * 60);
pub const DAEMON_IDLE_DELAY: Duration = Duration::from_secs(30 * 60);
/// A closing runtime finishes bounded local work (watcher, session, witness
/// handles); activation waits this long for it before reporting not ready.
const RETIRED_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub struct LifecycleTick {
    pub released: Vec<String>,
    pub exit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    NotAbsolute,
    NotCheckout,
    DifferentRepository,
    IdentityChanged,
    Unavailable,
    StorageBusy,
    CheckoutCapacity,
    RegistrationConflict,
}
impl SelectionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::StorageBusy => "storage_busy",
            Self::CheckoutCapacity => "checkout_capacity",
            Self::RegistrationConflict => "registration_conflict",
            _ => "workspace_selection_failed",
        }
    }
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotAbsolute => "not_absolute",
            Self::NotCheckout => "not_checkout",
            Self::DifferentRepository => "different_repository",
            Self::IdentityChanged => "identity_changed",
            Self::Unavailable => "unavailable",
            Self::StorageBusy => "storage_busy",
            Self::CheckoutCapacity => "checkout_capacity",
            Self::RegistrationConflict => "registration_conflict",
        }
    }
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Unavailable | Self::StorageBusy | Self::CheckoutCapacity
        )
    }
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
    pub scip: Option<PathBuf>,
    pub manifest: Option<PathBuf>,
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
            scip: None,
            manifest: None,
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
impl PriorHeadProbe {
    pub fn check(self) -> anyhow::Result<()> {
        let identity = self.identity.rediscover()?;
        let roots = match self.roots {
            Some(roots) => roots,
            None => TopologyRoots::production()?,
        };
        match Store::status_existing_readonly(roots, identity) {
            Ok(_) => Ok(()),
            // A cache tree without an admissible lock/format has no selected
            // head. Preserve cold W1's inert answer without creating state.
            Err(error)
                if error.to_string().starts_with("unsafe managed directory:")
                    || error
                        .to_string()
                        .starts_with("incompatible_index: missing or unsafe use lock")
                    || error
                        .to_string()
                        .starts_with("recovery_required: exceptional index format;") =>
            {
                anyhow::bail!("index_not_ready: no admitted head")
            }
            Err(error) => Err(error),
        }
    }
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
impl BrowserCheckoutsProbe {
    pub fn check(self) -> Vec<serde_json::Value> {
        let mut rows = std::collections::BTreeMap::new();
        let mut replacements = HashSet::new();
        let roots = self
            .roots
            .clone()
            .map(Ok)
            .unwrap_or_else(TopologyRoots::production);
        for (key, entry) in &self.entries {
            let index = roots
                .as_ref()
                .ok()
                .map(|roots| roots.cache.join("indexes").join(key).join("index.db"));
            let old_verified = entry.identity.rediscover().is_ok();
            // The old index belongs to the old inode and is never evidence for
            // its replacement. Listing is observational: a new same-key root is
            // selectable only after the old runtime has actually released.
            // browser_identity rechecks and performs the mutable retirement.
            let replaced_available = !old_verified
                && entry.replacement_idle
                && entry
                    .lost_reads
                    .as_ref()
                    .is_none_or(|reads| reads.load(std::sync::atomic::Ordering::Acquire) == 0)
                && WorkspaceIdentity::discover_unattached(
                    Some(&entry.identity.root),
                    &entry.identity.root,
                )
                .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                .is_ok_and(|new| {
                    new.root_key == *key
                        && !entry.identity.matches(&new)
                        && WorkspaceIdentity::discover_unattached(Some(&new.root), &new.root)
                            .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                            .is_ok_and(|found| {
                                !entry.identity.matches(&found)
                                    && found.record_id == new.record_id
                                    && (found.device, found.inode) == (new.device, new.inode)
                            })
                });
            let state = if replaced_available {
                replacements.insert(key.clone());
                "available"
            } else if !old_verified {
                "unavailable"
            } else if entry.runtime.is_some() {
                "available"
            } else if index.as_ref().is_some_and(|index| index.is_file()) {
                roots
                    .as_ref()
                    .ok()
                    .and_then(|roots| Store::browser_index_root_existing(roots, key).err())
                    .unwrap_or("available")
            } else {
                "available"
            };
            rows.insert(
                key.clone(),
                serde_json::json!({
                    "rootKey": key, "workspaceRoot": entry.identity.root,
                    "state": state,
                    "active": entry.active
                }),
            );
        }
        if let Ok(roots) = &roots
            && let Ok(indexes) = fs::read_dir(roots.cache.join("indexes"))
        {
            for candidate in indexes.flatten() {
                let key = candidate.file_name().to_string_lossy().to_string();
                if rows.contains_key(&key)
                    || key.len() != 64
                    || !key
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                {
                    continue;
                }
                let row = match Store::browser_index_root_existing(roots, &key) {
                    Ok(spelling)
                        if WorkspaceIdentity::discover_unattached(
                            Some(Path::new(&spelling)),
                            Path::new(&spelling),
                        )
                        .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                        .is_ok_and(|identity| identity.root_key == key) =>
                    {
                        serde_json::json!({"rootKey": key, "workspaceRoot": spelling, "state":"available", "active":false})
                    }
                    Ok(spelling) => {
                        serde_json::json!({"rootKey": key, "workspaceRoot": spelling, "state":"unavailable", "active":false})
                    }
                    Err(state) => {
                        serde_json::json!({"rootKey": key, "state":state, "active":false})
                    }
                };
                rows.insert(key, row);
            }
        }
        for (key, row) in &mut rows {
            row["revision"] = serde_json::Value::Null;
            row["catchingUp"] = serde_json::Value::Null;
            row["indexError"] = serde_json::Value::Null;
            if row["state"] != "available" {
                row["indexError"] = row["state"].clone();
                continue;
            }
            let entry = self.entries.get(key);
            // A selectable replacement has no claim on its predecessor's pin.
            if replacements.contains(key) {
                row["indexError"] = serde_json::json!("index_not_ready");
                continue;
            }
            let runtime = entry.and_then(|entry| entry.runtime.as_ref());
            let observation = (|| {
                if let Some(entry) = entry {
                    entry.identity.rediscover()?;
                }
                if let Some(runtime) = runtime {
                    let (response, catching_up) = runtime.evidence_response()?;
                    let revision = response.status()?.revision;
                    response.finish((revision, Some(catching_up)))
                } else {
                    let roots = roots
                        .as_ref()
                        .map_err(|_| anyhow::anyhow!("unavailable: topology roots unavailable"))?;
                    let spelling = row["workspaceRoot"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("unavailable: checkout root unavailable"))?;
                    let root = Path::new(spelling);
                    let identity = WorkspaceIdentity::discover_unattached(Some(root), root)?
                        .attach_existing_marker_readonly()?;
                    anyhow::ensure!(
                        identity.root_key == *key,
                        "root_changed: checkout key changed"
                    );
                    let status = Store::status_existing_readonly(roots.clone(), identity)?;
                    Ok((status.revision, None))
                }
            })();
            match observation {
                Ok((revision, catching_up)) => {
                    row["revision"] = serde_json::json!(revision);
                    row["catchingUp"] = serde_json::json!(catching_up);
                }
                Err(error) => {
                    let code = browser_index_error(&error);
                    row["indexError"] = serde_json::json!(code);
                    if code == "index_not_ready" {
                        row["catchingUp"] =
                            serde_json::json!(runtime.map(|runtime| runtime.catching_up()));
                    } else {
                        row["state"] = serde_json::json!(match code {
                            "storage_busy" => "storage_busy",
                            "corrupt" => "corrupt",
                            _ => "unavailable",
                        });
                    }
                }
            }
        }
        rows.into_values().collect()
    }
}

fn browser_index_error(error: &anyhow::Error) -> &'static str {
    if crate::store::transient_storage_contention(error) {
        return "storage_busy";
    }
    for cause in error.chain() {
        let text = cause.to_string();
        for code in [
            "root_changed",
            "root_key_collision",
            "workspace_id_changed",
            "index_not_ready",
        ] {
            if text.starts_with(code) {
                return code;
            }
        }
        if text.starts_with("incompatible_index")
            || text.starts_with("corrupt")
            || text.starts_with("recovery_required: exceptional index format")
        {
            return "corrupt";
        }
    }
    "unavailable"
}

/// Captured registry incarnations; preparation performs no registry mutation.
pub struct SelectionProbe {
    entries: HashMap<String, CheckoutMetadata>,
}
pub struct PreparedSelection {
    witness: SelectedCheckout,
    expected: Option<CheckoutMetadata>,
}
impl PreparedSelection {
    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.witness.identity
    }
}
impl SelectionProbe {
    pub fn prepare(
        &self,
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<PreparedSelection, SelectionError> {
        launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        let root = checkout_root(selected)?;
        // Membership must be established before marker attachment, which can create
        // a marker in a previously untouched linked worktree.
        let unattached = WorkspaceIdentity::discover_unattached(Some(&root), &root)
            .map_err(|_| SelectionError::Unavailable)?;
        let launch_common = common_identity(launch)?;
        let selected_common = common_identity_unattached(&unattached)?;
        if launch_common != selected_common {
            return Err(SelectionError::DifferentRepository);
        }
        let identity = unattached
            .attach_marker()
            .map_err(|_| SelectionError::Unavailable)?;
        // The worktree and its common directory can change while Git runs.
        launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        identity
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        if common_identity(launch)? != launch_common
            || common_identity(&identity)? != selected_common
        {
            return Err(SelectionError::IdentityChanged);
        }
        // Complete all fallible membership checks before reserving a session slot.
        // A failed selection must not attach or consume capacity.
        let witness = SelectedCheckout {
            launch: Arc::new(
                launch
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            ),
            identity: Arc::new(
                identity
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            ),
            launch_common,
            selected_common,
        };
        witness.before_answer()?;
        let expected = self.entries.get(&witness.root_key).cloned();
        Ok(PreparedSelection { witness, expected })
    }
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
    runtimes: HashMap<String, Arc<CheckoutRuntime>>,
    idle_exit_at: Option<Instant>,
    clock_override: Option<Instant>,
    /// Wakes the lifecycle owner: clients came or went, a runtime retired,
    /// accepted work drained or the last lease was released (Decision 0009 §5).
    lifecycle: Arc<tokio::sync::Notify>,
    #[cfg(feature = "test-support")]
    test_after_due_release: Option<Box<dyn FnOnce() + Send>>,
    #[cfg(feature = "test-support")]
    test_next_startup_hook: Option<Arc<dyn Fn() + Send + Sync>>,
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
            runtimes: HashMap::new(),
            idle_exit_at: Some(now + DAEMON_IDLE_DELAY),
            clock_override: None,
            lifecycle: Arc::new(tokio::sync::Notify::new()),
            #[cfg(feature = "test-support")]
            test_after_due_release: None,
            #[cfg(feature = "test-support")]
            test_next_startup_hook: None,
        }
    }

    /// Pause the next activation's startup reconciliation, installed before
    /// its worker starts.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_next_startup_hook_for_tests(&mut self, hook: Arc<dyn Fn() + Send + Sync>) {
        self.test_next_startup_hook = Some(hook);
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_after_due_release_for_tests(&mut self, hook: impl FnOnce() + Send + 'static) {
        self.test_after_due_release = Some(Box::new(hook));
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

    /// Probe only this registry's configured topology. A virgin checkout must not
    /// create derived state to answer a read, while a published head may activate.
    pub fn eligible_prior_head(&self, key: &str) -> anyhow::Result<()> {
        self.prior_head_probe(key)?.check()
    }

    /// Capture what a cold prior-head check needs, so the caller can release
    /// the registry lock before rediscovery and the SQLite open.
    pub fn prior_head_probe(&self, key: &str) -> anyhow::Result<PriorHeadProbe> {
        let entry = self
            .entries
            .get(key)
            .ok_or_else(|| anyhow::anyhow!("index_not_ready: checkout not attached"))?;
        Ok(PriorHeadProbe {
            identity: entry.identity.clone(),
            roots: self.roots.clone(),
        })
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
            #[cfg(feature = "test-support")]
            mcp_clock_skew_ms: std::sync::atomic::AtomicU64::new(0),
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
        scheduler.begin_startup_reconciliation();
        Ok(runtime)
    }

    /// Build a checkout's whole browser configuration from its registration
    /// (daemon defaults without one), or fail without a partial value.
    pub(crate) fn browser_config(
        &self,
        root: &Path,
        registration: Option<&CheckoutOptions>,
    ) -> anyhow::Result<http::BrowserConfig> {
        let config: BrowserOptions = registration
            .map(|registered| serde_json::from_value(registered.0.clone()))
            .transpose()?
            .unwrap_or_default();
        let mut options = IndexOptions::new(root.to_owned());
        options.scip_path = config.scip;
        options.manifest_path = config.manifest;
        options.max_file_bytes = config.max_file_bytes;
        // The raw registration preserves option presence. A supplied default
        // value (or an explicit null for an optional input) is still explicit;
        // comparing values would silently reuse an earlier checkout's inputs.
        let explicit_options = registration.is_some_and(|registered| {
            ["scip", "manifest", "maxFileBytes"]
                .iter()
                .any(|key| registered.0.get(*key).is_some())
        });
        let browse_root = config
            .browse_root
            .unwrap_or_else(|| options.workspace_root.clone());
        http::BrowserConfig::build(
            options,
            explicit_options,
            &browse_root,
            config.rust_source_roots,
            Some(crate::dependencies::CatalogOptions {
                cargo_home: config.cargo_home,
                rust_library: config.rust_library,
            }),
        )
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

    /// Browser selection is by the exact registered or discovered root key.
    /// No launch checkout, prior browser request, or serve registration is a default.
    pub fn browser_identity(&mut self, key: &str) -> Result<WorkspaceIdentity, SelectionError> {
        if key.len() != 64
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(SelectionError::NotCheckout);
        }
        if !self.entries.contains_key(key) {
            let roots = self
                .roots
                .clone()
                .map(Ok)
                .unwrap_or_else(TopologyRoots::production)
                .map_err(|_| SelectionError::Unavailable)?;
            let index = roots.cache.join("indexes").join(key).join("index.db");
            if !index.is_file() {
                return Err(SelectionError::NotCheckout);
            }
            let spelling = Store::browser_index_root_existing(&roots, key)
                .map_err(|_| SelectionError::Unavailable)?;
            let identity = WorkspaceIdentity::discover_unattached(
                Some(Path::new(&spelling)),
                Path::new(&spelling),
            )
            .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
            .map_err(|_| SelectionError::IdentityChanged)?;
            if identity.root_key != key {
                return Err(SelectionError::IdentityChanged);
            }
            self.register_discovered(&identity);
        }
        let entry = self.entries.get(key).ok_or(SelectionError::NotCheckout)?;
        if let Ok(identity) = entry.identity.rediscover() {
            return Ok(identity);
        }
        // A stale pathname key may now name a different checkout. Discovery
        // itself is read-only; never replace an attached, busy, or ambiguous
        // incarnation. The ordinary retirement gate owns all old resources.
        let root = entry.identity.root.clone();
        let current = WorkspaceIdentity::discover_unattached(Some(&root), &root)
            .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
            .map_err(|_| SelectionError::IdentityChanged)?;
        if current.root_key != key || entry.identity.matches(&current) {
            return Err(SelectionError::IdentityChanged);
        }
        self.retire_replaced(&current)?;
        self.register_discovered(&current);
        Ok(current)
    }

    fn register_discovered(&mut self, identity: &WorkspaceIdentity) {
        self.entries.insert(
            identity.root_key.clone(),
            Entry {
                identity: CheckoutMetadata::from_identity(identity),
                registration: None,
                sessions: HashSet::new(),
                released: true,
                pending_work: false,
                external_work: false,
                browser_until: None,
                release_at: None,
                root_lost: false,
                lost_reads: None,
            },
        );
    }

    /// Capture observational metadata only. Run `check` after releasing the registry.
    pub fn browser_checkouts_probe(&self) -> BrowserCheckoutsProbe {
        BrowserCheckoutsProbe {
            roots: self.roots.clone(),
            entries: self
                .entries
                .iter()
                .map(|(key, entry)| {
                    (
                        key.clone(),
                        BrowserCheckoutEntry {
                            identity: entry.identity.clone(),
                            active: self.runtimes.contains_key(key),
                            runtime: self.runtimes.get(key).cloned(),
                            replacement_idle: entry.released
                                && !self.runtimes.contains_key(key)
                                && entry.sessions.is_empty()
                                && entry.browser_until.is_none()
                                && !entry.pending_work
                                && !entry.external_work,
                            lost_reads: entry.lost_reads.clone(),
                        },
                    )
                })
                .collect(),
        }
    }

    /// Synchronous convenience for fixtures; daemon callers use the probe.
    pub fn browser_checkouts(&self) -> Vec<serde_json::Value> {
        self.browser_checkouts_probe().check()
    }

    pub fn active_count(&self) -> usize {
        self.entries
            .values()
            .filter(|entry| !entry.released || entry.pending_work)
            .count()
    }

    pub fn registration(&self, key: &str) -> Option<&CheckoutOptions> {
        self.entries.get(key)?.registration.as_ref()
    }

    /// Validate one inert browser configuration before changing any registry entry.
    pub fn validate_browser_options(
        identity: &WorkspaceIdentity,
        options: &CheckoutOptions,
    ) -> Result<(), SelectionError> {
        identity
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        let config: BrowserOptions =
            serde_json::from_value(options.0.clone()).map_err(|_| SelectionError::Unavailable)?;
        if config.max_file_bytes == 0
            || config.max_file_bytes > 16_777_216
            || config.manifest.is_some() && config.scip.is_none()
        {
            return Err(SelectionError::Unavailable);
        }
        let browse = config.browse_root.as_deref().unwrap_or(&identity.root);
        if !browse.is_dir() || !browse.is_absolute() {
            return Err(SelectionError::Unavailable);
        }
        if config.rust_source_roots.len() > 8 {
            return Err(SelectionError::Unavailable);
        }
        let mut labels = HashSet::new();
        for (label, root) in &config.rust_source_roots {
            if label.is_empty()
                || label.len() > 48
                || !label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                || !labels.insert(label)
                || !root.is_absolute()
                || !root.is_dir()
            {
                return Err(SelectionError::Unavailable);
            }
        }
        if let Some(compiler) = &config.trusted_rustc
            && (!compiler.is_absolute()
                || !compiler.is_file()
                || compiler
                    .canonicalize()
                    .map_err(|_| SelectionError::Unavailable)?
                    .starts_with(&identity.root))
        {
            return Err(SelectionError::Unavailable);
        }
        for path in [
            config.scip,
            config.manifest,
            config.rust_library,
            config.trusted_rustc,
            config.cargo_home,
        ]
        .into_iter()
        .flatten()
        {
            if !path.is_absolute() {
                return Err(SelectionError::Unavailable);
            }
        }
        Ok(())
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

    /// The provisioning caller holds the registry guard across this preflight
    /// and its later token commit. Retire a replaced old runtime *before* token
    /// creation: a read-only snapshot could pass during startup reconciliation retry backoff, then
    /// lose to a worker re-arm and fail registration after minting a token.
    pub fn can_register(
        &mut self,
        identity: &WorkspaceIdentity,
        options: &CheckoutOptions,
    ) -> Result<(), SelectionError> {
        identity
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        Self::validate_browser_options(identity, options)?;
        self.retire_replaced(identity)?;
        if self
            .entries
            .get(&identity.root_key)
            .is_some_and(|entry| Self::registration_conflicts(entry, options))
        {
            return Err(SelectionError::RegistrationConflict);
        }
        Ok(())
    }

    /// A checkout without a registration takes one in place, even while its
    /// runtime runs (Decision 0009 §6). A different existing registration
    /// conflicts until its checkout is released and idle.
    fn registration_conflicts(entry: &Entry, options: &CheckoutOptions) -> bool {
        entry
            .registration
            .as_ref()
            .is_some_and(|registered| registered != options)
            && (!entry.released || entry.pending_work || !entry.sessions.is_empty())
    }

    /// The configuration a registration publishes to a live runtime, always
    /// built in full before anything is bound or registered. None when it
    /// changes nothing or no runtime is live (activation builds it again).
    pub(crate) fn registration_config(
        &self,
        identity: &WorkspaceIdentity,
        options: &CheckoutOptions,
    ) -> anyhow::Result<Option<http::BrowserConfig>> {
        let key = &identity.root_key;
        // Always built, even for a repeated or already-recorded registration,
        // so a configuration that cannot build is refused before any
        // acknowledgement. Published only when it changes a live runtime.
        let config = self.browser_config(&identity.root, Some(options))?;
        let unchanged = self
            .entries
            .get(key)
            .is_some_and(|entry| entry.registration.as_ref() == Some(options));
        Ok((!unchanged && self.runtime(key).is_some()).then_some(config))
    }

    /// Record a registration and publish its configuration to a live runtime
    /// in one step under the registry guard. A conflict publishes nothing.
    pub(crate) fn register_in_place(
        &mut self,
        identity: &WorkspaceIdentity,
        options: CheckoutOptions,
        config: Option<http::BrowserConfig>,
    ) -> anyhow::Result<()> {
        self.register(identity, options)
            .map_err(|error| anyhow::anyhow!("{}", error.reason()))?;
        if let Some(config) = config
            && let Some(runtime) = self.runtime(&identity.root_key)
        {
            runtime.resources()?.scheduler.apply_config(config);
        }
        Ok(())
    }

    /// A repeat is idempotent. A different registration replaces an existing
    /// one only after release; a checkout without one takes it in place.
    pub fn register(
        &mut self,
        identity: &WorkspaceIdentity,
        options: CheckoutOptions,
    ) -> Result<(), SelectionError> {
        identity
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        self.retire_replaced(identity)?;
        let key = &identity.root_key;
        if let Some(entry) = self.entries.get_mut(key) {
            if !entry.identity.matches(identity) {
                return Err(SelectionError::IdentityChanged);
            }
            if entry.registration.as_ref() == Some(&options) {
                return Ok(());
            }
            if Self::registration_conflicts(entry, &options) {
                return Err(SelectionError::RegistrationConflict);
            }
            entry.registration = Some(options);
            return Ok(());
        }
        self.entries.insert(
            key.clone(),
            Entry {
                identity: CheckoutMetadata::from_identity(identity),
                registration: Some(options),
                sessions: HashSet::new(),
                released: true,
                pending_work: false,
                external_work: false,
                browser_until: None,
                release_at: None,
                root_lost: false,
                lost_reads: None,
            },
        );
        Ok(())
    }

    /// The caller retains its launch identity for the lifetime of its connection.
    /// This operation does not open a Store or elect a checkout leader.
    pub fn attach_launch(
        &mut self,
        session: u64,
        launch: &WorkspaceIdentity,
    ) -> Result<Arc<WorkspaceIdentity>, SelectionError> {
        launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        self.attach(Some(session), launch)
    }

    /// Capture entry incarnations before the slow filesystem/Git preparation.
    pub fn selection_probe(&self) -> SelectionProbe {
        SelectionProbe {
            entries: self
                .entries
                .iter()
                .map(|(key, entry)| (key.clone(), entry.identity.clone()))
                .collect(),
        }
    }

    /// Synchronous convenience for fixtures. Daemon callers prepare unlocked.
    pub fn select(
        &mut self,
        session: u64,
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<SelectedCheckout, SelectionError> {
        let prepared = self.selection_probe().prepare(launch, selected)?;
        self.select_prepared(session, prepared)
    }

    /// Commit prepared membership and Git checks performed outside the registry.
    /// Shared attachment retains its replacement rechecks and bounded retirement wait.
    pub fn select_prepared(
        &mut self,
        session: u64,
        prepared: PreparedSelection,
    ) -> Result<SelectedCheckout, SelectionError> {
        let PreparedSelection { witness, expected } = prepared;
        let key = &witness.root_key;
        let current = self.entries.get(key).map(|entry| &entry.identity);
        if current != expected.as_ref()
            && !current.is_some_and(|identity| identity.matches(&witness.identity))
        {
            return Err(SelectionError::IdentityChanged);
        }
        let identity = self.attach(Some(session), &witness.identity)?;
        Ok(SelectedCheckout {
            identity,
            ..witness
        })
    }

    /// Capacity is a resolved-checkout failure: recheck the selected root
    /// read-only before attributing the error, without reserving a slot.
    pub fn capacity_witness(
        &self,
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<SelectedCheckout, SelectionError> {
        Self::capacity_witness_unlocked(launch, selected)
    }

    pub fn capacity_witness_unlocked(
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<SelectedCheckout, SelectionError> {
        launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        let root = checkout_root(selected)?;
        let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root)
            .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
            .map_err(|_| SelectionError::IdentityChanged)?;
        let launch_common = common_identity(launch)?;
        let selected_common = common_identity(&identity)?;
        if launch_common != selected_common {
            return Err(SelectionError::IdentityChanged);
        }
        let witness = SelectedCheckout {
            launch: Arc::new(
                launch
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            ),
            identity: Arc::new(
                identity
                    .verified_clone()
                    .map_err(|_| SelectionError::IdentityChanged)?,
            ),
            launch_common,
            selected_common,
        };
        witness.before_answer()?;
        Ok(witness)
    }

    /// Mark a known checkout busy even without an attached client. The runtime
    /// supplies the actual queue/in-flight state and clears it after work drains.
    pub fn set_pending_work(&mut self, key: &str, pending: bool) -> Result<(), SelectionError> {
        let entry = self
            .entries
            .get_mut(key)
            .ok_or(SelectionError::Unavailable)?;
        entry.external_work = pending;
        entry.pending_work = pending;
        Ok(())
    }

    /// Probe only already-active checkouts. A failed queue probe remains busy;
    /// never activate an inert registered checkout to refresh pending work.
    pub fn refresh_pending_work(&mut self, key: &str) -> Result<bool, SelectionError> {
        let external = self
            .entries
            .get(key)
            .ok_or(SelectionError::Unavailable)?
            .external_work;
        let pending = external
            || match self.runtimes.get(key) {
                Some(runtime) => {
                    let resources = runtime
                        .resources()
                        .map_err(|_| SelectionError::Unavailable)?;
                    // Published state only; no SQLite under the registry lock.
                    // Watcher intent never holds a checkout (Decision 0009 §5).
                    startup_running(resources.scheduler.runtime_phase())
                        || resources.scheduler.leases() != 0
                        || resources.scheduler.fifo_pending()
                }
                None => {
                    return Ok(self
                        .entries
                        .get(key)
                        .ok_or(SelectionError::Unavailable)?
                        .pending_work);
                }
            };
        self.entries
            .get_mut(key)
            .ok_or(SelectionError::Unavailable)?
            .pending_work = pending;
        Ok(pending)
    }

    /// Release is a separate lifecycle decision, never implied by disconnect.
    /// The lifecycle owner calls this only after the idle delay.
    /// Returns true once the checkout is released. A live runtime is asked to
    /// retire; its worker retires it at a boundary (Decision 0009 §5) and
    /// wakes the lifecycle, which calls this again to finish.
    pub fn release(&mut self, key: &str) -> Result<bool, SelectionError> {
        self.refresh_pending_work(key)?;
        let entry = self.entries.get(key).ok_or(SelectionError::Unavailable)?;
        if !entry.sessions.is_empty() || entry.browser_until.is_some() || entry.pending_work {
            return Ok(false);
        }
        if let Some(runtime) = self.runtimes.get(key) {
            let resources = runtime
                .resources()
                .map_err(|_| SelectionError::Unavailable)?;
            match resources.scheduler.request_retire(|_| true) {
                http::RetireOutcome::Retired => self.finish_retired(key),
                http::RetireOutcome::Declined | http::RetireOutcome::Requested => {
                    return Ok(false);
                }
            }
        }
        self.entries.get_mut(key).unwrap().released = true;
        Ok(true)
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

    /// Called at MCP EOF/disconnect; an explicit worktree remains attached until then.
    pub fn disconnect(&mut self, session: u64) {
        self.disconnect_at(session, Instant::now());
    }

    pub fn disconnect_at(&mut self, session: u64, now: Instant) {
        let expired_at = self.expire_browsers(now);
        if let Some(expired_at) = expired_at {
            self.update_exit_deadline(expired_at);
        }
        let mut disconnected = false;
        for entry in self.entries.values_mut() {
            if entry.sessions.remove(&session) {
                disconnected = true;
                if entry.sessions.is_empty() && entry.browser_until.is_none() {
                    entry.release_at = Some(now + CHECKOUT_RELEASE_DELAY);
                }
            }
        }
        if !disconnected {
            return;
        }
        self.lifecycle.notify_one();
        if self
            .entries
            .values()
            .all(|entry| entry.sessions.is_empty() && entry.browser_until.is_none())
        {
            self.idle_exit_at = Some(now + DAEMON_IDLE_DELAY);
        } else {
            self.update_exit_deadline(now);
        }
    }

    /// Browser activity is a virtual client, not an MCP/CLI session. A later
    /// selected request renews its expiry without shortening the release grace.
    pub fn browser_request_at(
        &mut self,
        identity: &WorkspaceIdentity,
        now: Instant,
    ) -> Result<Arc<WorkspaceIdentity>, SelectionError> {
        let attached = self.attach(None, identity)?;
        let entry = self.entries.get_mut(&identity.root_key).unwrap();
        entry.browser_until = Some(now + BROWSER_IDLE_DELAY);
        entry.release_at = None;
        self.idle_exit_at = None;
        self.lifecycle.notify_one();
        Ok(attached)
    }

    /// Deterministic daemon-driver ticks for integration fixtures. Browser
    /// request timestamps still come from production HTTP's real clock.
    #[doc(hidden)]
    pub fn set_clock_override_for_tests(&mut self, now: Instant) {
        self.clock_override = Some(now);
        // Virtual time moved: the lifecycle owner re-evaluates its deadlines.
        self.lifecycle.notify_one();
    }

    /// Advance deterministic lifecycle clocks. Busy resources keep their original
    /// deadlines: once work drains, no extra grace interval is added.
    pub fn advance(&mut self, now: Instant) -> Result<LifecycleTick, SelectionError> {
        let now = self.clock_override.unwrap_or(now);
        // Expired clients disconnect at their deadline, not at this tick.
        let expired_at = self.expire_browsers(now);
        self.update_exit_deadline(expired_at.unwrap_or(now));
        let due: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.release_at.is_some_and(|at| now >= at))
            .map(|(key, _)| key.clone())
            .collect();
        let mut released = Vec::new();
        for key in due {
            if self.release(&key)? {
                self.entries.get_mut(&key).unwrap().release_at = None;
                released.push(key);
            }
        }
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.test_after_due_release.take() {
            hook();
        }
        let mut busy = false;
        for key in self.entries.keys().cloned().collect::<Vec<_>>() {
            busy |= self.refresh_pending_work(&key)?;
        }
        let exit = self.idle_exit_at.is_some_and(|at| now >= at)
            && !busy
            // Release may defer on startup reconciliation, a watcher, or an active read just before
            // the later pending-work refresh observes that work finish.
            && self.runtimes.is_empty()
            && self
                .entries
                .values()
                .all(|entry| entry.sessions.is_empty() && entry.browser_until.is_none());
        Ok(LifecycleTick { released, exit })
    }

    fn expire_browsers(&mut self, now: Instant) -> Option<Instant> {
        let mut last = None;
        for entry in self.entries.values_mut() {
            if entry.browser_until.is_some_and(|until| now >= until) {
                let until = entry.browser_until.take().unwrap();
                if entry.sessions.is_empty() {
                    entry.release_at = Some(until + CHECKOUT_RELEASE_DELAY);
                    last = Some(last.map_or(until, |previous: Instant| previous.max(until)));
                }
            }
        }
        last
    }

    pub fn idle_exit_deadline(&self) -> Option<Instant> {
        self.idle_exit_at
    }

    /// The next lifecycle deadline still ahead of `now`. A deadline already
    /// passed is pending on work, a lease or a retirement, and each of those
    /// signals the lifecycle when it changes.
    pub fn next_lifecycle_deadline(&self, now: Instant) -> Option<Instant> {
        // Under a test clock, virtual deadlines are not real-time sleeps: the
        // test moves the clock, which signals the lifecycle.
        if self.clock_override.is_some() {
            return None;
        }
        self.entries
            .values()
            .flat_map(|entry| [entry.release_at, entry.browser_until])
            .chain([self.idle_exit_at])
            .flatten()
            .filter(|at| *at > now)
            .min()
    }

    fn update_exit_deadline(&mut self, now: Instant) {
        if self
            .entries
            .values()
            .any(|entry| !entry.sessions.is_empty() || entry.browser_until.is_some())
        {
            self.idle_exit_at = None;
        } else if self.idle_exit_at.is_none() {
            self.idle_exit_at = Some(now + DAEMON_IDLE_DELAY);
        }
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
impl SelectedCheckout {
    pub fn before_answer(&self) -> Result<(), SelectionError> {
        self.before_answer_by(None)
    }
    /// `before_answer` whose Git lookups never wait past `by` (#131); one
    /// that cannot finish by then is `Unavailable`.
    pub fn before_answer_by(&self, by: Option<Instant>) -> Result<(), SelectionError> {
        let dirs = self.common_dirs_by(by)?;
        self.verify_at_send(&dirs)
    }
    /// The slow part of the final check (#131): discover both checkouts'
    /// common Git directories, each lookup waiting at most until `by`.
    pub fn common_dirs_by(&self, by: Option<Instant>) -> Result<CommonDirs, SelectionError> {
        self.launch.verify_readonly().map_err(identity_fault)?;
        self.identity.verify_readonly().map_err(identity_fault)?;
        let discover = |identity: &WorkspaceIdentity| {
            identity
                .git_common_dir_by(by)
                .map_err(|_| SelectionError::Unavailable)
        };
        Ok(CommonDirs {
            launch: discover(&self.launch)?,
            selected: discover(&self.identity)?,
        })
    }
    /// The send-time part of the final check (#131), filesystem calls only:
    /// both roots still verify, and the discovered common directories are
    /// still the captured one. A discovered directory that has gone is a
    /// proven change; any other I/O error proves nothing and is `Unavailable`.
    /// A Git pointer re-pointed in place, with every root and
    /// common-directory inode unchanged, is not seen here.
    pub fn verify_at_send(&self, dirs: &CommonDirs) -> Result<(), SelectionError> {
        self.launch.verify_readonly().map_err(identity_fault)?;
        self.identity.verify_readonly().map_err(identity_fault)?;
        let inode = |path: &Path| {
            fs::metadata(path)
                .map(|metadata| (metadata.dev(), metadata.ino()))
                .map_err(|error| io_fault(&error))
        };
        if inode(&dirs.launch)? != self.launch_common
            || inode(&dirs.selected)? != self.selected_common
            || self.launch_common != self.selected_common
        {
            return Err(SelectionError::IdentityChanged);
        }
        Ok(())
    }
}
/// A final check's failure is a proven change unless it is an I/O error
/// other than a missing path, such as `EACCES` or `EIO` (#131): that proves
/// nothing, so the check is `Unavailable` and runs again.
fn identity_fault(error: anyhow::Error) -> SelectionError {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map_or(SelectionError::IdentityChanged, io_fault)
}
fn io_fault(error: &std::io::Error) -> SelectionError {
    match error.kind() {
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory => {
            SelectionError::IdentityChanged
        }
        _ => SelectionError::Unavailable,
    }
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
    resources: std::sync::Mutex<Option<Arc<ActiveResources>>>,
    /// Released because its root was lost: a stale handle answers root_changed.
    root_lost: std::sync::atomic::AtomicBool,
    initial_indexing: bool,
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

/// A waiter does not change an accepted row on contention. Only direct,
/// transient lock failures are retried or converted to a typed expiry.
fn cli_index_contention(error: anyhow::Error, deadline: Instant) -> anyhow::Result<()> {
    if !crate::store::transient_storage_contention(&error)
        || !crate::store::cli_read_retryable_contention(&error)
    {
        return Err(error);
    }
    if Instant::now() >= deadline {
        return Err(crate::store::topology::StorageBusy.into());
    }
    Ok(())
}

fn cli_index_request(
    mut read: impl FnMut() -> anyhow::Result<Option<crate::store::requests::Request>>,
    deadline: Instant,
) -> anyhow::Result<Option<crate::store::requests::Request>> {
    loop {
        match read() {
            Ok(row) => return Ok(row),
            Err(error) => {
                cli_index_contention(error, deadline)?;
                std::thread::sleep(Duration::from_millis(20));
            }
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

    /// Invoked through a weak edge AFTER a queue tick releases all stream and
    /// watcher locks. This is a snapshot of the current verified startup reconciliation owner, not a
    /// claim that startup reconciliation ran again when a serving watcher gets a new epoch.
    #[cfg(feature = "test-support")]
    pub(crate) fn report_causal_startup_ready(&self, scheduler: &Arc<http::DaemonState>) {
        let Some(owner) = scheduler.checkout_serving_owner() else {
            return;
        };
        let Ok(resources) = self.resources() else {
            return;
        };
        if !Arc::ptr_eq(&resources.scheduler, scheduler) {
            return;
        }
        let Some(lineage) = scheduler.causal_lineage_for(&owner) else {
            return;
        };
        if lineage.owner_incarnation != owner.incarnation()
            || scheduler.causal_startup_already_reported(lineage)
            || owner.verify().is_err()
        {
            return;
        }
        let Ok(pin) = resources.store.index_baseline() else {
            return;
        };
        let still_ready = scheduler
            .checkout_serving_owner()
            .is_some_and(|current| Arc::ptr_eq(&current, &owner));
        if still_ready
            && owner.verify().is_ok()
            && scheduler.causal_lineage_for(&owner).is_some_and(|current| {
                current.owner_incarnation == lineage.owner_incarnation
                    && current.watch_epoch == lineage.watch_epoch
                    && current.ordinal == lineage.ordinal
            })
        {
            scheduler.emit_causal_startup_ready_once(lineage, pin);
        }
    }

    /// CLI calls share the activated checkout's Store and scheduler; they never
    /// reopen a second Store or independently elect a leader.
    pub fn cli_status(&self) -> anyhow::Result<serde_json::Value> {
        let (response, catching_up) = self.evidence_response()?;
        let mut status = response.status()?;
        status.catching_up = catching_up;
        status.watcher = Some(self.resources()?.scheduler.checkout_watcher_witness());
        Ok(serde_json::to_value(response.finish(status)?)?)
    }

    pub fn cli_symbols(&self, search: &str, limit: usize) -> anyhow::Result<serde_json::Value> {
        anyhow::ensure!((1..=150).contains(&limit), "limit must be 1..150");
        let (response, catching_up) = self.evidence_response()?;
        let (revision, items) = response.symbols_at(search, limit)?;
        response.finish(())?;
        Ok(serde_json::json!({"catchingUp": catching_up,
            "data": {"revision": revision, "items": items}}))
    }

    pub fn cli_query(&self, query: &crate::model::ViewQuery) -> anyhow::Result<serde_json::Value> {
        query.validate()?;
        let (response, catching_up) = self.evidence_response()?;
        let view = response
            .query_view(query)?
            .ok_or_else(|| anyhow::anyhow!("seed not found in current index"))?;
        Ok(serde_json::json!({"catchingUp": catching_up,
            "data": response.finish(view)?}))
    }

    pub fn cli_index(&self, options: &IndexOptions) -> anyhow::Result<serde_json::Value> {
        // Serialize acceptance with idle retirement. A caller may have cloned
        // this runtime before serve entered the registry gate: it must not
        // enqueue against resources the replacement has already retired.
        let resources = self.resources()?;
        // Durable acceptance is the single requests.db COMMIT. Only the
        // checkout worker may reconcile and claim this row. A CLI waiter
        // never creates a second writer or re-enqueues an uncertain job.
        let accepted = resources.scheduler.enqueue_index(options, None)?;
        let mut busy_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            // Record the terminal-commit count before re-reading, so a COMMIT
            // between the read and the wait is never missed (Decision 0009 §8).
            let seen = resources.store.terminal_generation();
            let row = cli_index_request(
                || resources.store.request_by_id(&accepted.id),
                busy_deadline,
            )?;
            busy_deadline = Instant::now() + Duration::from_secs(30);
            let row =
                row.ok_or_else(|| anyhow::anyhow!("storage_busy: accepted request disappeared"))?;
            match row.state.as_str() {
                "done" => {
                    let revision = row.revision.ok_or_else(|| {
                        anyhow::anyhow!("store_unavailable: completed request missing pin")
                    })?;
                    let (response, catching_up) = self.evidence_response()?;
                    response.validate_pin(revision)?;
                    let mut status = response.status()?;
                    status.catching_up = catching_up;
                    let status = response.finish(status)?;
                    return Ok(
                        serde_json::json!({"publishedRevision": revision, "status": status}),
                    );
                }
                "failed" => anyhow::bail!(
                    "{}: request {} failed; inspect job status",
                    row.error_code.as_deref().unwrap_or("store_unavailable"),
                    accepted.id
                ),
                "queued" | "running" => {
                    crate::index_coordinator::diagnostic_marker(
                        "-",
                        "cli_index_wait",
                        &format!("id={} state={} seen={seen}", accepted.id, row.state),
                    );
                    resources.store.wait_terminal_change(seen);
                    crate::index_coordinator::diagnostic_marker(
                        "-",
                        "cli_index_woke",
                        &format!("id={}", accepted.id),
                    );
                }
                _ => anyhow::bail!("store_unavailable: invalid request state"),
            }
        }
    }

    /// A retained runtime handle cannot keep the released Store or watcher alive.
    pub fn has_active_resources(&self) -> bool {
        self.resources.lock().unwrap().is_some()
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn notify_on_terminal_request_for_tests(
        &self,
        id: &str,
        notify: Arc<tokio::sync::Notify>,
    ) -> anyhow::Result<()> {
        self.resources()?
            .store
            .notify_on_terminal_request_for_tests(id, notify);
        Ok(())
    }

    /// Hold the checkout worker at its serial boundary while a fixture edits paths.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn hold_worker_step_for_tests(&self, hold: impl FnOnce()) {
        self.resources()
            .unwrap()
            .scheduler
            .hold_worker_step_for_tests(hold);
    }

    /// Make this checkout's next completion COMMIT durable but report an error.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn fail_next_completion_after_commit_for_tests(&self) {
        if let Ok(resources) = self.resources() {
            resources
                .store
                .fail_next_completion_after_commit_for_tests();
        }
    }

    #[cfg(feature = "test-support")]
    pub fn set_after_claim_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        if let Ok(resources) = self.resources() {
            resources.store.set_after_claim_hook_for_tests(hook);
        }
    }

    /// Pause startup reconciliation after old-root settlement and before it
    /// captures the workspace. The worker runs it on its next attempt.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_startup_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(resources) = self.resources() {
            resources.scheduler.set_startup_hook_for_tests(hook);
        }
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
                resources.scheduler.checkout_watch_pending() || resources.scheduler.fifo_pending()
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

    /// The clock MCP call deadlines are checked against: real time (or a
    /// paused test origin), plus virtual time advanced on this checkout.
    pub(crate) fn mcp_clock_now(&self) -> Instant {
        #[cfg(feature = "test-support")]
        let skew = Duration::from_millis(
            self.mcp_clock_skew_ms
                .load(std::sync::atomic::Ordering::Acquire),
        );
        #[cfg(not(feature = "test-support"))]
        let skew = Duration::ZERO;
        #[cfg(feature = "test-support")]
        if let Some(origin) = *self.mcp_call_clock_origin.lock().unwrap() {
            return origin + skew;
        }
        Instant::now() + skew
    }

    /// Pause mandatory call time; share this clock with the test transport.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn freeze_mcp_call_clock_for_tests(&self, origin: Instant) {
        *self.mcp_call_clock_origin.lock().unwrap() = Some(origin);
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn mcp_call_clock_now_for_tests(&self) -> Instant {
        self.mcp_clock_now()
    }

    /// Freeze only optional hint accounting. Mandatory call deadlines keep using real time.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    /// Freeze the optional-hint clock: it then reads the call's acceptance
    /// instant plus the advanced skew, so the hint budget and the call
    /// deadline share one origin however long acceptance took (#131).
    pub fn freeze_mcp_hint_clock_for_tests(&self) {
        *self.mcp_hint_clock_origin.lock().unwrap() = Some(Instant::now());
    }

    /// The optional-hint clock for a call accepted at `accepted`.
    pub(crate) fn mcp_hint_clock_now(&self, accepted: Instant) -> Instant {
        #[cfg(feature = "test-support")]
        if self.mcp_hint_clock_origin.lock().unwrap().is_some() {
            return accepted
                + Duration::from_millis(
                    self.mcp_clock_skew_ms
                        .load(std::sync::atomic::Ordering::Acquire),
                );
        }
        let _ = accepted;
        self.mcp_clock_now()
    }

    /// Hold this checkout's next MCP selected read until `hook` returns.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_mcp_read_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        *self.mcp_read_hook.lock().unwrap() = Some(Box::new(hook));
    }

    /// Run `gate` at the start of every MCP selected read on this checkout.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_mcp_read_gate_for_tests(&self, gate: Arc<dyn Fn() + Send + Sync>) {
        *self.mcp_read_gate.lock().unwrap() = Some(gate);
    }

    /// Run `gate` after every MCP read admission on this checkout.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_mcp_admitted_gate_for_tests(&self, gate: Arc<dyn Fn() + Send + Sync>) {
        *self.mcp_admitted_gate.lock().unwrap() = Some(gate);
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn run_mcp_admitted_gate_for_tests(&self) {
        let gate = self.mcp_admitted_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate();
        }
    }

    #[cfg(feature = "test-support")]
    pub(crate) fn run_mcp_read_hook_for_tests(&self) {
        let gate = self.mcp_read_gate.lock().unwrap().clone();
        if let Some(gate) = gate {
            gate();
        }
        let hook = self.mcp_read_hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Advance this checkout's MCP call clock without waiting real time.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn advance_mcp_clock_for_tests(&self, by: Duration) {
        let millis = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.mcp_clock_skew_ms
            .fetch_add(millis, std::sync::atomic::Ordering::AcqRel);
    }

    /// Inspect this runtime's existing recovery flag in optional MCP expiry fixtures.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn mcp_recovery_pending_for_tests(&self) -> anyhow::Result<bool> {
        Ok(self.resources()?.store.recovery_pending_for_tests())
    }

    #[cfg(feature = "test-support")]
    pub fn mcp_busy_reads_for_tests(&self, count: usize) -> anyhow::Result<()> {
        self.resources()?.store.mcp_busy_reads_for_tests(count);
        Ok(())
    }

    /// Read the published watcher health; it never waits for an inventory.
    pub fn mcp_watcher_degraded(&self) -> anyhow::Result<bool> {
        Ok(self.resources()?.scheduler.checkout_watcher_degraded())
    }

    /// Admit W1 through the same selected response path; a missing head is not corruption.
    pub fn mcp_describe(
        &self,
        selected: Option<&SelectedCheckout>,
    ) -> anyhow::Result<(
        Option<CheckoutEvidence>,
        bool,
        &'static str,
        Option<serde_json::Value>,
    )> {
        let admitted = match selected {
            Some(selected) => self.evidence_for_selected(selected),
            None => self.evidence_response(),
        };
        match admitted {
            Ok((response, catching_up)) => Ok((Some(response), catching_up, "ready", None)),
            Err(error)
                if error
                    .downcast_ref::<crate::store::topology::IndexNotReady>()
                    .is_some() =>
            {
                let catching_up = self.catching_up();
                if self.initial_indexing {
                    let started = self.resources().is_ok_and(|resources| {
                        !matches!(
                            resources.scheduler.runtime_phase(),
                            RuntimePhase::Activating
                                | RuntimePhase::Reconciling { started: false, .. }
                        )
                    });
                    let progress =
                        (!started).then(|| serde_json::json!({"completed":0,"total":null}));
                    Ok((None, catching_up, "indexing", progress))
                } else {
                    Ok((None, catching_up, "reconciling", None))
                }
            }
            Err(error) => Err(error),
        }
    }

    pub fn evidence_response(&self) -> anyhow::Result<(CheckoutEvidence, bool)> {
        let resources = self.resources()?;
        let lease = resources.scheduler.admission_lease()?;
        let catching_up = Self::catching_up_in(&resources, resources.scheduler.runtime_phase());
        let response = resources.store.evidence_response_daemon_local()?;
        Ok((
            CheckoutEvidence {
                response,
                _lease: lease,
                selection: None,
            },
            catching_up,
        ))
    }

    /// One admitted read, one projection, and one final selected-read fence.
    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn metrics_memo_entries_for_tests(&self) -> anyhow::Result<usize> {
        Ok(self.resources()?.store.metrics_memo_entries_for_tests())
    }

    pub fn metrics_response(
        &self,
        options: &crate::store::MetricsOptions,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        hooks: MetricsResponseHooks<'_>,
        max_bytes: usize,
    ) -> anyhow::Result<(Vec<u8>, bool)> {
        self.projected_response(check, hooks, max_bytes, |response, check| {
            response.metrics_at(options, &mut || check())
        })
    }

    pub fn architecture_response(
        &self,
        options: &crate::store::MetricsOptions,
        check: &mut (impl FnMut() -> anyhow::Result<()> + Send),
        hooks: MetricsResponseHooks<'_>,
        max_bytes: usize,
    ) -> anyhow::Result<(Vec<u8>, bool)> {
        self.projected_response(check, hooks, max_bytes, |response, check| {
            response.architecture_view(options, &mut || check())
        })
    }

    fn projected_response<P: serde::Serialize, F: FnMut() -> anyhow::Result<()> + Send>(
        &self,
        check: &mut F,
        hooks: MetricsResponseHooks<'_>,
        max_bytes: usize,
        materialize: impl FnOnce(
            &CheckoutEvidence,
            &mut (dyn FnMut() -> anyhow::Result<()> + Send),
        ) -> anyhow::Result<P>,
    ) -> anyhow::Result<(Vec<u8>, bool)> {
        use serde::Serialize;
        struct Bounded<'a, F> {
            bytes: Vec<u8>,
            check: &'a mut F,
            on_write: &'a mut dyn FnMut(usize),
            max_bytes: usize,
            failure: Option<anyhow::Error>,
        }
        impl<F: FnMut() -> anyhow::Result<()>> std::io::Write for Bounded<'_, F> {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                (self.on_write)(self.bytes.len());
                if let Err(error) = (self.check)() {
                    self.failure = Some(error);
                    return Err(std::io::Error::other("metrics check failed"));
                }
                if data.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
                    self.failure = Some(anyhow::anyhow!("response_too_large"));
                    return Err(std::io::Error::other("response_too_large"));
                }
                self.bytes.extend_from_slice(data);
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Envelope<'a, P> {
            catching_up: bool,
            payload: &'a P,
        }
        let (response, catching_up) = self.evidence_response()?;
        (hooks.before_work)();
        check()?;
        let payload = match materialize(&response, &mut || {
            (hooks.on_progress)();
            check()
        }) {
            Ok(payload) => payload,
            Err(error) => {
                // Located config failures are public evidence too. Fence and bound
                // them before the HTTP layer preserves the structured diagnostic.
                if let Some(diagnostic) = crate::rules::diagnostic(&error) {
                    crate::metrics::serialize_checked(
                        &serde_json::json!({"error": diagnostic}),
                        max_bytes,
                        check,
                    )?;
                    (hooks.before_finish)();
                    check()?;
                    response.finish(())?;
                    (hooks.after_finish)();
                    check()?;
                }
                return Err(error);
            }
        };
        let mut writer = Bounded {
            bytes: Vec::new(),
            check,
            on_write: hooks.on_write,
            max_bytes,
            failure: None,
        };
        let encoded = serde_json::to_writer(
            &mut writer,
            &Envelope {
                catching_up,
                payload: &payload,
            },
        );
        if let Some(error) = writer.failure.take() {
            return Err(error);
        }
        encoded.map_err(|_| anyhow::anyhow!("bounded_work: metrics serialization failed"))?;
        (writer.check)()?;
        (hooks.before_finish)();
        (writer.check)()?;
        let bytes = response.finish(writer.bytes)?;
        (hooks.after_finish)();
        (writer.check)()?;
        Ok((bytes, catching_up))
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

fn common_identity(identity: &WorkspaceIdentity) -> Result<(u64, u64), SelectionError> {
    common_identity_by(identity, None)
}
fn common_identity_by(
    identity: &WorkspaceIdentity,
    by: Option<Instant>,
) -> Result<(u64, u64), SelectionError> {
    let path = identity
        .git_common_dir_by(by)
        .map_err(|_| SelectionError::Unavailable)?;
    let metadata = fs::metadata(path).map_err(|_| SelectionError::Unavailable)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Probe membership before writing a worktree marker. Git output is bounded and
/// the root identity is checked on both sides of the probe.
fn common_identity_unattached(identity: &WorkspaceIdentity) -> Result<(u64, u64), SelectionError> {
    common_identity_unattached_with_executable(identity, Path::new("git"))
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

/// Walk the literal selected pathname. In particular, do not canonicalize a
/// symlink (including one in an intermediate component) into a different root.
fn checkout_root(selected: &Path) -> Result<PathBuf, SelectionError> {
    if !selected.is_absolute() {
        return Err(SelectionError::NotAbsolute);
    }
    let mut literal = PathBuf::new();
    for component in selected.components() {
        match component {
            Component::RootDir | Component::Normal(_) => literal.push(component),
            _ => return Err(SelectionError::NotCheckout),
        }
        let metadata = fs::symlink_metadata(&literal).map_err(|_| SelectionError::Unavailable)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(SelectionError::NotCheckout);
        }
    }
    for candidate in literal.ancestors() {
        match fs::symlink_metadata(candidate.join(".git")) {
            Ok(metadata) if metadata.is_dir() || metadata.is_file() => {
                if metadata.file_type().is_symlink() {
                    return Err(SelectionError::NotCheckout);
                }
                return Ok(candidate.to_path_buf());
            }
            Ok(_) => return Err(SelectionError::NotCheckout),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(_) => return Err(SelectionError::Unavailable),
        }
    }
    Err(SelectionError::NotCheckout)
}

#[cfg(test)]
mod selection_tests {
    use super::*;
    use anyhow::Context;
    use std::process::Command;

    #[test]
    fn cli_index_contention_retries_the_same_accepted_request() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let accepted = store.enqueue_request(&options, None).unwrap();
        let mut attempts = 0;
        let row = cli_index_request(
            || {
                attempts += 1;
                if attempts == 1 {
                    return Err(crate::store::topology::StorageBusy.into());
                }
                store.request_by_id(&accepted.id)
            },
            Instant::now() + Duration::from_secs(30),
        )
        .unwrap()
        .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(row.id, accepted.id);
        assert_eq!(row.seq, accepted.seq);
        assert_eq!(row.state, "queued");
        assert_eq!(row.revision, None);
        assert_eq!(row.error_code, None);
        let queue = store.earliest_unfinished_request().unwrap().unwrap();
        assert_eq!(queue.id, accepted.id);
        assert_eq!(queue.seq, accepted.seq);
        let next = store.enqueue_request(&options, None).unwrap();
        assert_eq!(next.seq, accepted.seq + 1, "waiter must never re-enqueue");
    }

    #[test]
    fn cli_index_expired_contention_is_typed_and_keeps_accepted_row_pending() {
        let state = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let store = Store::open_for_tests(state.path(), workspace.path()).unwrap();
        let options = IndexOptions::new(workspace.path().to_owned());
        let accepted = store.enqueue_request(&options, None).unwrap();
        let before = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(before.state, "queued");

        let busy = || -> anyhow::Error {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            )
            .into()
        };
        let expired = Instant::now() - Duration::from_millis(1);
        let error = cli_index_contention(busy(), expired).unwrap_err();
        assert!(
            error
                .downcast_ref::<crate::store::topology::StorageBusy>()
                .is_some()
        );
        assert_eq!(error.to_string(), "storage_busy");
        // Neither an invariant-busy string nor a guarded SQLite cause is retried.
        let invariant = anyhow::anyhow!("storage_busy: cached claim changed");
        assert_eq!(
            cli_index_contention(invariant, expired)
                .unwrap_err()
                .to_string(),
            "storage_busy: cached claim changed"
        );
        let guarded = Err::<(), _>(busy())
            .context("root_changed: request root replaced")
            .unwrap_err();
        assert_eq!(
            cli_index_contention(guarded, expired)
                .unwrap_err()
                .to_string(),
            "root_changed: request root replaced"
        );
        let integrity = Err::<(), _>(busy())
            .context("incompatible_index: selected integrity")
            .unwrap_err();
        assert_eq!(
            cli_index_contention(integrity, expired)
                .unwrap_err()
                .to_string(),
            "incompatible_index: selected integrity"
        );

        let after = store.request_by_id(&accepted.id).unwrap().unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.seq, before.seq);
        assert_eq!(after.state, before.state);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.error_code, before.error_code);
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn listing_probe_releases_registry_before_slow_work() {
        let temp = tempfile::tempdir().unwrap();
        let roots =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let registry = Arc::new(std::sync::Mutex::new(CheckoutRegistry::with_roots(roots)));
        let (captured_tx, captured_rx) = std::sync::mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
        let worker_registry = registry.clone();
        let worker = std::thread::spawn(move || {
            let probe = worker_registry.lock().unwrap().browser_checkouts_probe();
            captured_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            probe.check()
        });
        captured_rx
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap();
        assert_eq!(registry.try_lock().unwrap().active_count(), 0);
        resume_tx.send(()).unwrap();
        assert!(worker.join().unwrap().is_empty());
    }

    #[test]
    fn selection_preparation_does_not_hold_registry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
        let root = root.canonicalize().unwrap();
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let registry = Arc::new(std::sync::Mutex::new(CheckoutRegistry::new()));
        let (captured_tx, captured_rx) = std::sync::mpsc::sync_channel(0);
        let (resume_tx, resume_rx) = std::sync::mpsc::sync_channel(0);
        let worker_registry = registry.clone();
        let worker = std::thread::spawn(move || {
            let probe = worker_registry.lock().unwrap().selection_probe();
            captured_tx.send(()).unwrap();
            resume_rx.recv().unwrap();
            probe.prepare(&identity, &root)
        });
        captured_rx
            .recv_timeout(crate::testing::HANG_BACKSTOP)
            .unwrap();
        assert_eq!(registry.try_lock().unwrap().active_count(), 0);
        resume_tx.send(()).unwrap();
        let prepared = worker.join().unwrap().unwrap();
        assert_eq!(registry.lock().unwrap().active_count(), 0);
        registry
            .lock()
            .unwrap()
            .select_prepared(1, prepared)
            .unwrap();
        assert_eq!(registry.lock().unwrap().active_count(), 1);
    }

    #[test]
    fn prepared_selection_accepts_a_concurrent_same_identity_attachment() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
        let root = root.canonicalize().unwrap();
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let mut registry = CheckoutRegistry::new();
        let prepared = registry
            .selection_probe()
            .prepare(&identity, &root)
            .unwrap();
        registry.attach_launch(2, &identity).unwrap();
        let selected = registry.select_prepared(1, prepared).unwrap();
        assert_eq!(selected.root_key, identity.root_key);
        assert_eq!(registry.active_count(), 1);
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(
            registry.entries.get(&identity.root_key).unwrap().sessions,
            HashSet::from([1, 2])
        );
    }

    #[test]
    fn prepared_selection_cannot_replace_a_successor_incarnation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
        let root = root.canonicalize().unwrap();
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let mut registry = CheckoutRegistry::new();
        let prepared = registry
            .selection_probe()
            .prepare(&identity, &root)
            .unwrap();
        let mut successor = CheckoutMetadata::from_identity(&identity);
        successor.record_id = "successor".to_owned();
        registry.entries.insert(
            identity.root_key.clone(),
            Entry {
                identity: successor.clone(),
                registration: None,
                sessions: HashSet::from([2]),
                released: false,
                pending_work: false,
                external_work: false,
                browser_until: None,
                release_at: None,
                root_lost: false,
                lost_reads: None,
            },
        );
        assert!(matches!(
            registry.select_prepared(1, prepared),
            Err(SelectionError::IdentityChanged)
        ));
        let entry = registry.entries.get(&identity.root_key).unwrap();
        assert!(entry.identity == successor);
        assert_eq!(entry.sessions, HashSet::from([2]));
    }

    #[test]
    fn unattached_git_wait_is_bounded_with_open_or_closed_stdout() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
        let root = root.canonicalize().unwrap();
        let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root).unwrap();
        let executable = temp.path().join("stalled-git");
        for close_stdout in [false, true] {
            let close = if close_stdout { "exec 1>&-\n" } else { "" };
            fs::write(
                &executable,
                format!("#!/bin/sh\n{close}exec /bin/sleep 5\n"),
            )
            .unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let start = Instant::now();
            assert_eq!(
                common_identity_unattached_with_executable(&identity, &executable),
                Err(SelectionError::Unavailable),
                "close_stdout={close_stdout}"
            );
            assert!(start.elapsed() < crate::testing::HANG_BACKSTOP);
        }
        assert!(!root.join(".git/trellis/workspace-id").exists());
    }

    #[test]
    fn unattached_git_wait_preserves_common_identity_without_a_marker() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
        let root = root.canonicalize().unwrap();
        let identity = WorkspaceIdentity::discover_unattached(Some(&root), &root).unwrap();
        let common = fs::metadata(root.join(".git")).unwrap();
        assert_eq!(
            common_identity_unattached(&identity),
            Ok((common.dev(), common.ino()))
        );
        assert!(!root.join(".git/trellis/workspace-id").exists());
    }

    #[test]
    fn rejected_selection_never_attaches_or_creates_a_foreign_marker() {
        let temp = tempfile::tempdir().unwrap();
        let launch = temp.path().join("launch");
        let foreign = temp.path().join("foreign");
        git(temp.path(), &["init", "--quiet", launch.to_str().unwrap()]);
        git(temp.path(), &["init", "--quiet", foreign.to_str().unwrap()]);
        let launch = launch.canonicalize().unwrap();
        let foreign = foreign.canonicalize().unwrap();
        let identity = WorkspaceIdentity::discover(Some(&launch), &launch).unwrap();
        let mut registry = CheckoutRegistry::new();
        registry.attach_launch(17, &identity).unwrap();
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(
            registry.select(17, &identity, &foreign).unwrap_err(),
            SelectionError::DifferentRepository
        );
        assert!(!foreign.join(".git/trellis/workspace-id").exists());
        assert_eq!(
            registry
                .select(17, &identity, Path::new("relative"))
                .unwrap_err(),
            SelectionError::NotAbsolute
        );
        assert_eq!(registry.entries.len(), 1);
        assert!(
            registry
                .entries
                .get(&identity.root_key)
                .unwrap()
                .sessions
                .contains(&17)
        );
        registry.disconnect(17);
        assert!(
            registry
                .entries
                .values()
                .all(|entry| entry.sessions.is_empty())
        );
    }
    #[test]
    fn capacity_failure_retains_verified_root_without_reserving_a_slot() {
        let temp = tempfile::tempdir().unwrap();
        let launch = temp.path().join("launch");
        git(temp.path(), &["init", "--quiet", launch.to_str().unwrap()]);
        git(
            &launch,
            &["commit", "--allow-empty", "--quiet", "-m", "fixture"],
        );
        let selected = temp.path().join("linked");
        git(
            &launch,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                selected.to_str().unwrap(),
                "HEAD",
            ],
        );
        let launch = launch.canonicalize().unwrap();
        let selected = selected.canonicalize().unwrap();
        let identity = WorkspaceIdentity::discover(Some(&launch), &launch).unwrap();
        let mut registry = CheckoutRegistry::new();
        registry.attach_launch(17, &identity).unwrap();
        let prepared = registry
            .selection_probe()
            .prepare(&identity, &selected)
            .unwrap();
        for index in 0..MAX_ACTIVE_CHECKOUTS - 1 {
            let key = format!("fixture-{index}");
            let mut metadata = CheckoutMetadata::from_identity(&identity);
            metadata.root_key = key.clone();
            registry.entries.insert(
                key,
                Entry {
                    identity: metadata,
                    registration: None,
                    sessions: HashSet::from([18]),
                    released: false,
                    pending_work: false,
                    external_work: false,
                    browser_until: None,
                    release_at: None,
                    root_lost: false,
                    lost_reads: None,
                },
            );
        }
        assert_eq!(registry.active_count(), MAX_ACTIVE_CHECKOUTS);
        assert_eq!(
            registry.select_prepared(17, prepared).unwrap_err(),
            SelectionError::CheckoutCapacity
        );
        assert_eq!(registry.entries.len(), MAX_ACTIVE_CHECKOUTS);
        assert_eq!(
            registry
                .capacity_witness(&identity, &selected)
                .unwrap()
                .root,
            selected
        );
        assert!(
            registry
                .entries
                .values()
                .all(|entry| entry.sessions.contains(&17)
                    == (entry.identity.root_key == identity.root_key))
        );
    }
}
