//! Browser registration: checkout identity by root key, registered options and the browser
//! configuration built from them.
use super::{
    BrowserOptions, CheckoutMetadata, CheckoutOptions, CheckoutRegistry, Entry, SelectionError,
};
use crate::http;
use crate::indexer::IndexOptions;
use crate::store::Store;
use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
use std::collections::HashSet;
use std::path::Path;

impl CheckoutRegistry {
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
        options.max_file_bytes = config.max_file_bytes;
        let explicit_options =
            registration.is_some_and(|registered| registered.0.get("maxFileBytes").is_some());
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
        if config.max_file_bytes == 0 || config.max_file_bytes > 16_777_216 {
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
        for path in [config.rust_library, config.trusted_rustc, config.cargo_home]
            .into_iter()
            .flatten()
        {
            if !path.is_absolute() {
                return Err(SelectionError::Unavailable);
            }
        }
        Ok(())
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
}
