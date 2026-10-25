//! A checkout's browser configuration as one immutable, versioned value
//! (Decision 0009 §6). `serve` replaces it in place; handlers pin one version.

use crate::dependencies::CatalogOptions;
use crate::indexer::IndexOptions;
use std::sync::Arc;

pub struct BrowserConfig {
    /// Daemon defaults for index work, or `serve`'s registered options.
    pub options: IndexOptions,
    /// Registered index options were given explicitly and win over recorded ones.
    pub explicit_options: bool,
    pub browser: crate::file_tree::SourceDir,
    pub rust_sources: Vec<crate::rust_sources::Root>,
    pub dependency_options: Option<CatalogOptions>,
}

impl BrowserConfig {
    /// Build and validate every part, or fail without a partial value.
    pub fn build(
        options: IndexOptions,
        explicit_options: bool,
        browse_root: &std::path::Path,
        source_roots: Vec<(String, std::path::PathBuf)>,
        dependency_options: Option<CatalogOptions>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            options,
            explicit_options,
            browser: crate::file_tree::SourceDir::open(browse_root)?,
            rust_sources: crate::rust_sources::open_roots(source_roots)?,
            dependency_options,
        })
    }
}

impl super::DaemonState {
    /// The current configuration. A handler takes it once and uses that
    /// version for its whole request.
    pub fn config(&self) -> Arc<BrowserConfig> {
        self.config.read().unwrap().clone()
    }

    /// Publish a new configuration as one step (Decision 0009 §6). Accepted
    /// requests keep the options recorded with them; explicit index options
    /// are applied by the worker's next full watcher inventory, and the
    /// dependency catalog rebuilds for the new catalog options.
    pub(crate) fn apply_config(self: &Arc<Self>, config: BrowserConfig) {
        let explicit = config
            .explicit_options
            .then(|| Arc::new(config.options.clone()));
        let enabled = config.dependency_options.is_some();
        {
            // Swap and reset the catalog under its lock, so a refresh that
            // checked the old version cannot schedule against the new one.
            let mut index = self.dependencies.lock().unwrap();
            *self.config.write().unwrap() = Arc::new(config);
            index
                .cancel
                .store(true, std::sync::atomic::Ordering::Release);
            index.cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
            index.generation += 1;
            index.catalog = None;
            index.warnings.clear();
            index.state = if enabled { "loading" } else { "disabled" };
        }
        *self.explicit_pending.lock().unwrap() = explicit;
        self.inbox.wake();
    }
}
