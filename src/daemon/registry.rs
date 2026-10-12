//! In-memory checkout admission. Retained entries never own checkout resources.
use crate::store::{
    Store,
    topology::{TopologyRoots, WorkspaceIdentity, WriterSession},
};
use crate::{
    http, index_coordinator::establish_serving_session, indexer::IndexOptions, model::CancelFlag,
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Values are retained as data. No browser provider or checkout descriptor is
/// opened by registration; activation uses only its selected checkout's values.
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
    pub jev_budget_dir: Option<PathBuf>,
    pub jev_budget_cents: Option<u64>,
    pub jev_key: Option<String>,
    pub acp_runner: Option<PathBuf>,
    pub acp_state_dir: Option<PathBuf>,
    pub acp_max_attempts: Option<u64>,
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
            jev_budget_dir: None,
            jev_budget_cents: None,
            jev_key: None,
            acp_runner: None,
            acp_state_dir: None,
            acp_max_attempts: None,
        }
    }
}

#[derive(Clone)]
struct CheckoutMetadata {
    root: PathBuf,
    root_key: String,
    record_id: String,
    device: u64,
    inode: u64,
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

struct Entry {
    identity: CheckoutMetadata,
    registration: Option<CheckoutOptions>,
    sessions: HashSet<u64>,
    released: bool,
    pending_work: bool,
    external_work: bool,
    browser_until: Option<Instant>,
    release_at: Option<Instant>,
}

pub struct CheckoutRegistry {
    entries: HashMap<String, Entry>,
    roots: Option<TopologyRoots>,
    runtimes: HashMap<String, Arc<CheckoutRuntime>>,
    idle_exit_at: Option<Instant>,
    orphan_scan_cache: Option<(Instant, bool)>,
    orphan_scan_count: u64,
    clock_override: Option<Instant>,
    #[cfg(all(test, feature = "test-support"))]
    test_jev_endpoint: Option<String>,
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
            orphan_scan_cache: None,
            orphan_scan_count: 0,
            clock_override: None,
            #[cfg(all(test, feature = "test-support"))]
            test_jev_endpoint: None,
        }
    }

    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn set_jev_endpoint_for_tests(&mut self, endpoint: String) {
        self.test_jev_endpoint = Some(endpoint);
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
        if let Some(runtime) = self.runtimes.get(key) {
            return Ok(runtime.clone());
        }
        let identity = entry.identity.rediscover()?;
        let roots = match &self.roots {
            Some(roots) => roots.clone(),
            None => TopologyRoots::production()?,
        };
        let store = Store::open(roots, identity)?;
        // Publish only a proved-virgin queue before this runtime is visible to
        // selected status reads. Those reads otherwise compete with the EX
        // stage needed before mandatory H can begin.
        store.prepare_virgin_queue_before_activation()?;
        let config: BrowserOptions = entry
            .registration
            .as_ref()
            .map(|registered| serde_json::from_value(registered.0.clone()))
            .transpose()?
            .unwrap_or_default();
        let mut options = IndexOptions::new(entry.identity.root.clone());
        options.scip_path = config.scip;
        options.manifest_path = config.manifest;
        options.max_file_bytes = config.max_file_bytes;
        // The raw registration preserves option presence. A supplied default
        // value (or an explicit null for an optional input) is still explicit;
        // comparing values would silently reuse an earlier checkout's inputs.
        let explicit_options = entry.registration.as_ref().is_some_and(|registered| {
            ["scip", "manifest", "maxFileBytes"]
                .iter()
                .any(|key| registered.0.get(*key).is_some())
        });
        let provider = match (config.jev_budget_dir, config.jev_budget_cents) {
            (Some(dir), Some(cap)) => {
                let key = config
                    .jev_key
                    .or_else(|| std::env::var("JEV_KEY").ok())
                    .ok_or_else(|| anyhow::anyhow!("JEV_KEY must be configured"))?;
                let live = crate::live_jev::LiveJev::open(&dir, key, cap, &options.workspace_root)?;
                #[cfg(all(test, feature = "test-support"))]
                let live = if let Some(endpoint) = &self.test_jev_endpoint {
                    live.with_test_endpoint(endpoint.clone())
                } else {
                    live
                };
                Some(Arc::new(live))
            }
            _ => None,
        };
        let acp = match (
            config.acp_runner,
            config.acp_state_dir,
            config.acp_max_attempts,
        ) {
            (Some(runner), Some(state_dir), Some(max_attempts)) => {
                Some(Arc::new(crate::acp::Acp::open(crate::acp::AcpConfig {
                    runner,
                    state_dir,
                    max_attempts,
                    workspace: options.workspace_root.clone(),
                })?))
            }
            _ => None,
        };
        // Browser registration remains inert until activation. The scheduler's
        // internal token/address are not bound or exposed by browser provisioning.
        let scheduler = http::new_with_dependency_options(
            store.clone(),
            options.clone(),
            format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            "127.0.0.1:1".parse()?,
            provider,
            acp,
            config
                .browse_root
                .unwrap_or_else(|| options.workspace_root.clone()),
            config.rust_source_roots,
            Some(crate::dependencies::CatalogOptions {
                cargo_home: config.cargo_home,
                rust_library: config.rust_library,
            }),
        )?;
        let runtime = Arc::new(CheckoutRuntime {
            resources: std::sync::Mutex::new(Some(Arc::new(ActiveResources { store, scheduler }))),
            phase: std::sync::Mutex::new(RuntimePhase::Reconciling),
            active: AtomicBool::new(true),
            active_reads: Arc::new(AtomicUsize::new(0)),
            last_error: std::sync::Mutex::new(None),
            h_in_flight: AtomicBool::new(true),
            retry_waiting: AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            pre_h_hook: std::sync::Mutex::new(None),
        });
        #[cfg(feature = "test-support")]
        if let Some(reporter) = super::causal_witness::CausalWitness::from_env(key.to_owned()) {
            runtime
                .resources()?
                .scheduler
                .set_causal_witness_runtime(reporter, Arc::downgrade(&runtime));
        }
        self.runtimes.insert(key.to_owned(), runtime.clone());
        runtime.start(options, explicit_options);
        Ok(runtime)
    }

    pub fn runtime(&self, key: &str) -> Option<Arc<CheckoutRuntime>> {
        self.runtimes.get(key).cloned()
    }

    /// The lifecycle owner calls this only after the idle deadline. A failed
    /// queue probe is busy, never evidence that it is safe to release a leader.
    fn release_runtime(&mut self, key: &str) -> anyhow::Result<bool> {
        let Some(runtime) = self.runtimes.get(key).cloned() else {
            return Ok(true);
        };
        let resources = runtime.resources()?;
        if runtime.h_in_flight.load(Ordering::Acquire)
            || (runtime.catching_up() && !runtime.retry_waiting.load(Ordering::Acquire))
            || runtime.active_reads.load(Ordering::Acquire) != 0
            || CheckoutRuntime::queue_pending(&resources)
        {
            return Ok(false);
        }
        let mut phase = runtime.phase.lock().unwrap();
        CheckoutRuntime::refresh_phase(&mut phase, &resources);
        if runtime.h_in_flight.load(Ordering::Acquire)
            || runtime.active_reads.load(Ordering::Acquire) != 0
        {
            return Ok(false);
        }
        runtime.active.store(false, Ordering::Release);
        resources.scheduler.release_checkout_runtime();
        *phase = RuntimePhase::Reconciling;
        runtime.resources.lock().unwrap().take();
        resources.store.retire_checkout_sqlite_witnesses();
        drop(phase);
        drop(resources);
        self.runtimes.remove(key);
        Ok(true)
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
            },
        );
    }

    /// Inspect existing indexes without activating a checkout or changing clocks.
    pub fn browser_checkouts(&self) -> Vec<serde_json::Value> {
        let mut rows = std::collections::BTreeMap::new();
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
                && entry.released
                && !self.runtimes.contains_key(key)
                && WorkspaceIdentity::discover_unattached(
                    Some(&entry.identity.root),
                    &entry.identity.root,
                )
                .and_then(WorkspaceIdentity::attach_existing_marker_readonly)
                .is_ok_and(|new| {
                    new.root_key == *key
                        && !entry.identity.matches(&new)
                        && self.can_retire_replaced(&new)
                });
            let state = if replaced_available {
                "available"
            } else if !old_verified {
                "unavailable"
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
                    "active": self.runtimes.contains_key(key)
                }),
            );
        }
        if let Ok(roots) = roots
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
                let row = match Store::browser_index_root_existing(&roots, &key) {
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
        rows.into_values().collect()
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
        if config.jev_budget_dir.is_some() != config.jev_budget_cents.is_some()
            || config.acp_runner.is_some() != config.acp_state_dir.is_some()
            || config.acp_runner.is_some() != config.acp_max_attempts.is_some()
            || config
                .jev_budget_cents
                .is_some_and(|cap| !(10..=500).contains(&cap))
            || config
                .acp_max_attempts
                .is_some_and(|cap| !(1..=20).contains(&cap))
        {
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
        if let Some(dir) = &config.jev_budget_dir {
            let environment_key = std::env::var("JEV_KEY").ok();
            let key = config
                .jev_key
                .as_deref()
                .or(environment_key.as_deref())
                .ok_or(SelectionError::Unavailable)?;
            if key.trim().is_empty()
                || reqwest::header::HeaderValue::from_str(&format!("Bearer {key}")).is_err()
            {
                return Err(SelectionError::Unavailable);
            }
            match fs::symlink_metadata(dir) {
                Ok(m) => {
                    if !m.is_dir()
                        || m.file_type().is_symlink()
                        || m.uid() != unsafe { libc::geteuid() }
                        || m.mode() & 0o777 != 0o700
                    {
                        return Err(SelectionError::Unavailable);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(SelectionError::Unavailable),
            }
        }
        if let (Some(runner), Some(state)) = (&config.acp_runner, &config.acp_state_dir) {
            let workspace = identity
                .root
                .canonicalize()
                .map_err(|_| SelectionError::Unavailable)?;
            let runner = runner
                .canonicalize()
                .map_err(|_| SelectionError::Unavailable)?;
            if !runner.is_file()
                || runner.starts_with(&workspace)
                || state
                    .components()
                    .any(|c| matches!(c, Component::ParentDir))
            {
                return Err(SelectionError::Unavailable);
            }
            let mut ancestor = state.as_path();
            while !ancestor
                .try_exists()
                .map_err(|_| SelectionError::Unavailable)?
            {
                ancestor = ancestor.parent().ok_or(SelectionError::Unavailable)?;
            }
            if ancestor
                .canonicalize()
                .map_err(|_| SelectionError::Unavailable)?
                .starts_with(&workspace)
            {
                return Err(SelectionError::Unavailable);
            }
            match fs::symlink_metadata(state) {
                Ok(m) => {
                    if !m.is_dir()
                        || m.file_type().is_symlink()
                        || m.uid() != unsafe { libc::geteuid() }
                        || m.mode() & 0o777 != 0o700
                    {
                        return Err(SelectionError::Unavailable);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(SelectionError::Unavailable),
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
            config.jev_budget_dir,
            config.acp_runner,
            config.acp_state_dir,
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
        if !self.can_retire_replaced(identity) {
            return Err(SelectionError::IdentityChanged);
        }
        if !self.release(key)? {
            return Err(SelectionError::IdentityChanged);
        }
        self.entries.remove(key);
        Ok(())
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
        // Verify a *new* identity independently: rediscovering old metadata
        // deliberately rejects the changed record_id after `git init`.
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
    /// creation: a read-only snapshot could pass during H retry backoff, then
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
        // An earlier CLI read/index may have activated the checkout with no
        // browser settings. Once its work drains, release that runtime before
        // registering serve-scoped providers and source roots.
        if self.entries.get(&identity.root_key).is_some_and(|entry| {
            entry.registration.is_none()
                && entry.sessions.is_empty()
                && entry.browser_until.is_none()
                && self.runtimes.contains_key(&identity.root_key)
        }) && !self.release(&identity.root_key)?
        {
            // A CLI-only runtime may still have an H, selected read, or queue
            // operation in flight. It must retire before serve installs its
            // options, but it is not a conflicting browser registration.
            return Err(SelectionError::StorageBusy);
        }
        if let Some(entry) = self.entries.get(&identity.root_key)
            && entry.registration.as_ref() != Some(options)
            && (entry.registration.is_some() || self.runtimes.contains_key(&identity.root_key))
            && (!entry.released || entry.pending_work || !entry.sessions.is_empty())
        {
            return Err(SelectionError::RegistrationConflict);
        }
        Ok(())
    }

    /// A repeat is idempotent. Replacement is possible only after release.
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
            if (entry.registration.is_some() || self.runtimes.contains_key(key))
                && (!entry.released || entry.pending_work || !entry.sessions.is_empty())
            {
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

    pub fn select(
        &mut self,
        session: u64,
        launch: &WorkspaceIdentity,
        selected: &Path,
    ) -> Result<SelectedCheckout, SelectionError> {
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
        let selected = self.attach(Some(session), &identity)?;
        Ok(SelectedCheckout {
            identity: selected,
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
                    (runtime.catching_up() && !runtime.retry_waiting.load(Ordering::Acquire))
                        || runtime.active_reads.load(Ordering::Acquire) != 0
                        || CheckoutRuntime::queue_pending(&resources)
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
    pub fn release(&mut self, key: &str) -> Result<bool, SelectionError> {
        self.refresh_pending_work(key)?;
        let entry = self.entries.get(key).ok_or(SelectionError::Unavailable)?;
        if !entry.sessions.is_empty() || entry.browser_until.is_some() || entry.pending_work {
            return Ok(false);
        }
        if !self
            .release_runtime(key)
            .map_err(|_| SelectionError::Unavailable)?
        {
            return Ok(false);
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
        Ok(attached)
    }

    /// Deterministic daemon-driver ticks for integration fixtures. Browser
    /// request timestamps still come from production HTTP's real clock.
    #[doc(hidden)]
    pub fn set_clock_override_for_tests(&mut self, now: Instant) {
        self.clock_override = Some(now);
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
        let mut busy = false;
        for key in self.entries.keys().cloned().collect::<Vec<_>>() {
            busy |= self.refresh_pending_work(&key)?;
        }
        let candidate = self.idle_exit_at.is_some_and(|at| now >= at)
            && !busy
            && self
                .entries
                .values()
                .all(|entry| entry.sessions.is_empty() && entry.browser_until.is_none());
        // Never scan every index directory on the routine 250-ms tick. A
        // genuinely pending orphan is checked again after a bounded interval.
        let exit = if candidate {
            let pending = match self.orphan_scan_cache {
                Some((until, pending)) if now < until => pending,
                _ => {
                    let pending = self.orphan_queue_pending();
                    self.orphan_scan_count += 1;
                    self.orphan_scan_cache = Some((now + Duration::from_secs(5), pending));
                    pending
                }
            };
            !pending
        } else {
            false
        };
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

    /// Check durable queues even for roots that have not attached since daemon
    /// startup. An unreadable queue is busy, not permission to abandon work.
    fn orphan_queue_pending(&self) -> bool {
        let roots = match self
            .roots
            .clone()
            .map(Ok)
            .unwrap_or_else(TopologyRoots::production)
        {
            Ok(roots) => roots,
            Err(_) => return true,
        };
        let active: HashSet<String> = self.runtimes.keys().cloned().collect();
        Store::orphan_queues_pending(&roots, &active)
    }

    #[doc(hidden)]
    pub fn orphan_scan_count_for_tests(&self) -> u64 {
        self.orphan_scan_count
    }

    pub fn idle_exit_deadline(&self) -> Option<Instant> {
        self.idle_exit_at
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
        self.launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        self.identity
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        if common_identity(&self.launch)? != self.launch_common
            || common_identity(&self.identity)? != self.selected_common
            || self.launch_common != self.selected_common
        {
            return Err(SelectionError::IdentityChanged);
        }
        self.launch
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        self.identity
            .verify_readonly()
            .map_err(|_| SelectionError::IdentityChanged)?;
        Ok(())
    }
}
impl std::ops::Deref for SelectedCheckout {
    type Target = WorkspaceIdentity;
    fn deref(&self) -> &Self::Target {
        &self.identity
    }
}

/// Keeps a selected immutable snapshot alive through final workspace and
/// revision checks; idle release cannot drop its owner before this guard drops.
pub struct CheckoutEvidence {
    response: crate::store::EvidenceResponse,
    reads: Arc<AtomicUsize>,
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
impl Drop for CheckoutEvidence {
    fn drop(&mut self) {
        self.reads.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Resources are removed from the slot on release, even if a client retains
/// the inert CheckoutRuntime handle after disconnect.
struct ActiveResources {
    store: Store,
    scheduler: Arc<http::DaemonState>,
}

/// One activated checkout has its own native stream and no process-global leader.
/// A failed H retains no claim authority; the worker retries while attached.
pub struct CheckoutRuntime {
    resources: std::sync::Mutex<Option<Arc<ActiveResources>>>,
    phase: std::sync::Mutex<RuntimePhase>,
    active: AtomicBool,
    active_reads: Arc<AtomicUsize>,
    last_error: std::sync::Mutex<Option<String>>,
    h_in_flight: AtomicBool,
    retry_waiting: AtomicBool,
    #[cfg(feature = "test-support")]
    pre_h_hook: std::sync::Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

enum RuntimePhase {
    Reconciling,
    Ready(Arc<WriterSession>),
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

impl CheckoutRuntime {
    fn resources(&self) -> anyhow::Result<Arc<ActiveResources>> {
        self.resources
            .lock()
            .unwrap()
            .as_ref()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("index_not_ready: checkout released"))
    }

    pub fn browser_scheduler(&self) -> anyhow::Result<Arc<http::DaemonState>> {
        Ok(self.resources()?.scheduler.clone())
    }

    /// Invoked through a weak edge AFTER a queue tick releases all stream and
    /// watcher locks. This is a snapshot of the current verified H owner, not a
    /// claim that H ran again when a serving watcher gets a new epoch.
    pub(crate) fn report_causal_h_ready(&self, scheduler: &Arc<http::DaemonState>) {
        if !self.active.load(Ordering::Acquire) || self.h_in_flight.load(Ordering::Acquire) {
            return;
        }
        let owner = match &*self.phase.lock().unwrap() {
            RuntimePhase::Ready(owner) => owner.clone(),
            _ => return,
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
            || scheduler.causal_h_already_reported(lineage)
            || owner.verify().is_err()
        {
            return;
        }
        let Ok(pin) = resources.store.index_baseline() else {
            return;
        };
        let still_ready = matches!(&*self.phase.lock().unwrap(),
            RuntimePhase::Ready(current) if Arc::ptr_eq(current, &owner));
        if still_ready
            && self.active.load(Ordering::Acquire)
            && !self.h_in_flight.load(Ordering::Acquire)
            && owner.verify().is_ok()
            && scheduler.causal_lineage_for(&owner).is_some_and(|current| {
                current.owner_incarnation == lineage.owner_incarnation
                    && current.watch_epoch == lineage.watch_epoch
                    && current.ordinal == lineage.ordinal
            })
        {
            scheduler.emit_causal_h_ready_once(lineage, pin);
        }
    }

    /// CLI calls share the activated checkout's Store and scheduler; they never
    /// reopen a second Store or independently elect a leader.
    pub fn cli_status(&self) -> anyhow::Result<serde_json::Value> {
        let (response, catching_up) = self.evidence_response()?;
        let mut status = response.status()?;
        status.catching_up = catching_up;
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

    pub fn cli_export(&self) -> anyhow::Result<serde_json::Value> {
        let (response, catching_up) = self.evidence_response()?;
        let graph = response.graph()?;
        Ok(serde_json::json!({"catchingUp": catching_up, "data": response.finish(graph)?}))
    }

    pub fn cli_index(&self, options: &IndexOptions) -> anyhow::Result<serde_json::Value> {
        let resources = self.resources()?;
        // Durable acceptance is the single requests.db COMMIT. Only the
        // checkout scheduler may elect, reconcile and claim this row. A CLI
        // waiter never creates a second leader or re-enqueues an uncertain job.
        let accepted = resources.store.enqueue_request(options, None)?;
        let mut busy_deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let row = match resources.store.request_by_id(&accepted.id) {
                Ok(row) => {
                    busy_deadline = Instant::now() + Duration::from_secs(30);
                    row
                }
                Err(error) => {
                    cli_index_contention(error, busy_deadline)?;
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
            }
            .ok_or_else(|| anyhow::anyhow!("storage_busy: accepted request disappeared"))?;
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
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => anyhow::bail!("store_unavailable: invalid request state"),
            }
        }
    }

    /// A retained runtime handle cannot keep the released Store or watcher alive.
    pub fn has_active_resources(&self) -> bool {
        self.resources.lock().unwrap().is_some()
    }

    /// Reconcile a captured phase with the scheduler's current *verified*
    /// Only the daemon's committed H session can serve as the local writer.
    fn refresh_phase(phase: &mut RuntimePhase, resources: &ActiveResources) {
        match resources.scheduler.checkout_serving_owner() {
            Some(owner) => {
                if !matches!(phase, RuntimePhase::Ready(previous) if Arc::ptr_eq(previous, &owner))
                {
                    *phase = RuntimePhase::Ready(owner);
                }
            }
            None => *phase = RuntimePhase::Reconciling,
        }
    }

    fn queue_pending(resources: &ActiveResources) -> bool {
        if resources.scheduler.checkout_root_loss_retired() {
            !matches!(resources.store.old_root_unfinished_request(), Ok(None))
        } else {
            !matches!(resources.store.earliest_unfinished_request(), Ok(None))
        }
    }

    #[cfg(feature = "test-support")]
    pub fn set_after_claim_hook_for_tests(&self, hook: impl FnOnce() + Send + 'static) {
        if let Ok(resources) = self.resources() {
            resources.store.set_after_claim_hook_for_tests(hook);
        }
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_pre_h_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.pre_h_hook.lock().unwrap() = Some(hook);
    }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn set_h_hook_for_tests(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(resources) = self.resources() {
            resources.scheduler.set_checkout_h_hook_for_tests(hook);
        }
    }

    pub fn reconciliation_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }

    #[doc(hidden)]
    pub fn retry_waiting_for_tests(&self) -> bool {
        self.retry_waiting.load(Ordering::Acquire) && !self.h_in_flight.load(Ordering::Acquire)
    }

    fn catching_up_in_phase(&self, phase: &RuntimePhase, resources: &ActiveResources) -> bool {
        if self.h_in_flight.load(Ordering::Acquire) {
            return true;
        }
        if resources.scheduler.checkout_root_loss_retired() {
            return Self::queue_pending(resources);
        }
        !matches!(phase, RuntimePhase::Ready(_))
            || resources.scheduler.checkout_watch_pending()
            || Self::queue_pending(resources)
    }

    pub fn catching_up(&self) -> bool {
        let Ok(resources) = self.resources() else {
            return true;
        };
        let mut phase = self.phase.lock().unwrap();
        Self::refresh_phase(&mut phase, &resources);
        self.catching_up_in_phase(&phase, &resources)
    }

    /// Sample freshness and admit the selected prior head under one runtime gate.
    /// A validated prior head admits reads, never durable mutations or work
    /// scheduling. Only this runtime's committed daemon-local H grants that
    /// authority; the advisory catchingUp flag is not an admission gate.
    pub(crate) fn require_mutation_ready(&self) -> anyhow::Result<()> {
        let mut phase = self.phase.lock().unwrap();
        anyhow::ensure!(
            self.active.load(Ordering::Acquire),
            "index_not_ready: checkout released"
        );
        let resources = self.resources()?;
        anyhow::ensure!(
            !resources.store.root_path_replaced()?,
            "root_changed: captured root lost"
        );
        Self::refresh_phase(&mut phase, &resources);
        let RuntimePhase::Ready(session) = &*phase else {
            return Err(crate::store::topology::IndexNotReady::new(
                "mandatory reconciliation not committed",
            )
            .into());
        };
        resources.store.verify_committed_writer_claim(session)
    }

    pub fn evidence_response(&self) -> anyhow::Result<(CheckoutEvidence, bool)> {
        let mut phase = self.phase.lock().unwrap();
        anyhow::ensure!(
            self.active.load(Ordering::Acquire),
            "index_not_ready: checkout released"
        );
        let resources = self.resources()?;
        Self::refresh_phase(&mut phase, &resources);
        let catching_up = self.catching_up_in_phase(&phase, &resources);
        let response = resources.store.evidence_response_daemon_local()?;
        self.active_reads.fetch_add(1, Ordering::AcqRel);
        Ok((
            CheckoutEvidence {
                response,
                reads: self.active_reads.clone(),
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

    fn start(self: &Arc<Self>, options: IndexOptions, explicit_options: bool) {
        let runtime = self.clone();
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(250);
            loop {
                let worker = runtime.clone();
                let options = options.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    worker.reconcile(&options, explicit_options)
                })
                .await;
                match outcome {
                    Ok(Ok(session)) => {
                        // Reconciliation cannot settle after release: release
                        // requires the phase to be Ready, set only here.
                        if let Ok(resources) = runtime.resources() {
                            resources.scheduler.retain_serving_session(session.clone());
                        } else {
                            break;
                        }
                        *runtime.phase.lock().unwrap() = RuntimePhase::Ready(session);
                        *runtime.last_error.lock().unwrap() = None;
                        runtime.h_in_flight.store(false, Ordering::Release);
                        break;
                    }
                    result => {
                        {
                            let mut failure = runtime.last_error.lock().unwrap();
                            if failure.is_none() {
                                *failure = Some(match result {
                                    Ok(Err(error)) => format!("{error:#}"),
                                    Err(error) => error.to_string(),
                                    _ => unreachable!(),
                                });
                            }
                        }
                        if runtime.retire_lost_root_h() {
                            runtime.h_in_flight.store(false, Ordering::Release);
                            break;
                        }
                        // The active worker has finished. During backoff an empty
                        // idle checkout can release; a queued row or live read
                        // still pins its owner. Re-arm only under the phase lock,
                        // paired with release's final gate.
                        runtime.retry_waiting.store(true, Ordering::Release);
                        runtime.h_in_flight.store(false, Ordering::Release);
                        tokio::time::sleep(backoff).await;
                        backoff = backoff.saturating_mul(2).min(Duration::from_secs(30));
                        let _phase = runtime.phase.lock().unwrap();
                        if !runtime.active.load(Ordering::Acquire) {
                            break;
                        }
                        runtime.h_in_flight.store(true, Ordering::Release);
                        runtime.retry_waiting.store(false, Ordering::Release);
                    }
                }
            }
        });
    }

    /// Finish old-root durable work while the elected H owner is still held.
    /// On an inconclusive queue write keep that owner and retry; never call an
    /// empty ordinary old-root probe proof of completion.
    fn retire_lost_root_h(&self) -> bool {
        let Ok(resources) = self.resources() else {
            return false;
        };
        if !resources.store.root_path_replaced().is_ok_and(|lost| lost) {
            return false;
        }
        // A lost captured root never publishes H or claims again. The old
        // queue remains a durable obligation until its terminal COMMIT is
        // confirmed, independent of any former writer token.
        if resources.store.fail_changed_root_requests().is_err() {
            return false;
        }
        *self.phase.lock().unwrap() = RuntimePhase::Reconciling;
        true
    }

    fn reconcile(
        &self,
        options: &IndexOptions,
        explicit_options: bool,
    ) -> anyhow::Result<Arc<WriterSession>> {
        let resources = self.resources()?;
        let store = &resources.store;
        anyhow::ensure!(
            !store.root_path_replaced()?,
            "root_changed: captured root lost"
        );
        store.settle_foreign_root_requests()?;
        #[cfg(feature = "test-support")]
        if let Some(hook) = self.pre_h_hook.lock().unwrap().take() {
            hook();
        }
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let admitted_options = if store.is_recreate_pending() {
            if explicit_options {
                Some(options.clone())
            } else {
                // Corrupt RecreatePending needs accepted same-root options.
                // RootReplaced has a verified new root: after old-root queue
                // settlement, its explicit daemon defaults can start H when
                // no accepted new-root head supplies options.
                let head = store
                    .earliest_unfinished_request()?
                    .map(|request| request.options(Path::new(store.workspace_root())))
                    .transpose()?;
                head.or_else(|| store.is_root_replaced().then(|| options.clone()))
            }
        } else if explicit_options || store.recorded_index_options()?.is_none() {
            Some(options.clone())
        } else {
            None
        };
        establish_serving_session(store, admitted_options.as_ref(), &cancel)
    }
}

fn common_identity(identity: &WorkspaceIdentity) -> Result<(u64, u64), SelectionError> {
    let path = identity
        .git_common_dir()
        .map_err(|_| SelectionError::Unavailable)?;
    let metadata = fs::metadata(path).map_err(|_| SelectionError::Unavailable)?;
    Ok((metadata.dev(), metadata.ino()))
}

/// Probe membership before writing a worktree marker. Git output is bounded and
/// the root identity is checked on both sides of the probe.
fn common_identity_unattached(identity: &WorkspaceIdentity) -> Result<(u64, u64), SelectionError> {
    let root = &identity.root;
    let before = fs::symlink_metadata(root).map_err(|_| SelectionError::Unavailable)?;
    if !before.is_dir()
        || before.file_type().is_symlink()
        || (before.dev(), before.ino()) != (identity.device, identity.inode)
    {
        return Err(SelectionError::IdentityChanged);
    }
    let mut child = std::process::Command::new("git")
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
    let reader = std::thread::spawn(move || {
        use std::io::Read;
        let mut output = Vec::new();
        stdout.take(4097).read_to_end(&mut output).map(|_| output)
    });
    let deadline = Instant::now() + Duration::from_millis(750);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::yield_now(),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SelectionError::Unavailable);
            }
        }
    };
    let output = reader
        .join()
        .map_err(|_| SelectionError::Unavailable)?
        .map_err(|_| SelectionError::Unavailable)?;
    if !status.success() || output.len() > 4096 {
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

    #[tokio::test]
    async fn serve_registration_cannot_replace_busy_cli_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        git(temp.path(), &["init", "--quiet", root.to_str().unwrap()]);
        let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
        let roots =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let mut registry = CheckoutRegistry::with_roots(roots);
        let options = CheckoutOptions(serde_json::json!({"browseRoot": identity.root}));
        registry.attach_launch(41, &identity).unwrap();
        let runtime = registry.activate(&identity.root_key).unwrap();
        assert_eq!(
            registry.can_register(&identity, &options),
            Err(SelectionError::RegistrationConflict)
        );
        assert!(registry.runtimes.contains_key(&identity.root_key));
        registry.disconnect(41);
        runtime.active_reads.fetch_add(1, Ordering::AcqRel);
        assert_eq!(
            registry.can_register(&identity, &options),
            Err(SelectionError::StorageBusy)
        );
        assert!(registry.runtimes.contains_key(&identity.root_key));
        runtime.active_reads.fetch_sub(1, Ordering::AcqRel);
        runtime.h_in_flight.store(true, Ordering::Release);
        assert_eq!(
            registry.can_register(&identity, &options),
            Err(SelectionError::StorageBusy)
        );
        assert!(registry.runtimes.contains_key(&identity.root_key));
        // A rejected call cannot install browser settings or change the writer.
        assert!(registry.entries[&identity.root_key].registration.is_none());
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
                },
            );
        }
        assert_eq!(registry.active_count(), MAX_ACTIVE_CHECKOUTS);
        assert_eq!(
            registry.select(17, &identity, &selected).unwrap_err(),
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
