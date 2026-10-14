//! A checkout's browser configuration as one immutable, versioned value
//! (Decision 0009 §6). `serve` replaces it in place; handlers pin one version.

use crate::acp::Acp;
use crate::dependencies::CatalogOptions;
use crate::indexer::IndexOptions;
use crate::live_jev::LiveJev;
use std::sync::Arc;

pub struct BrowserConfig {
    /// Daemon defaults for index work, or `serve`'s registered options.
    pub options: IndexOptions,
    /// Registered index options were given explicitly and win over recorded ones.
    pub explicit_options: bool,
    pub browser: crate::file_tree::SourceDir,
    pub rust_sources: Vec<crate::rust_sources::Root>,
    pub dependency_options: Option<CatalogOptions>,
    pub provider: Option<Arc<LiveJev>>,
    pub acp: Option<Arc<Acp>>,
}

impl BrowserConfig {
    /// Build and validate every part, or fail without a partial value.
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        options: IndexOptions,
        explicit_options: bool,
        browse_root: &std::path::Path,
        source_roots: Vec<(String, std::path::PathBuf)>,
        dependency_options: Option<CatalogOptions>,
        provider: Option<Arc<LiveJev>>,
        acp: Option<Arc<Acp>>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            options,
            explicit_options,
            browser: crate::file_tree::SourceDir::open(browse_root)?,
            rust_sources: crate::rust_sources::open_roots(source_roots)?,
            dependency_options,
            provider,
            acp,
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
        let explicit = config.explicit_options.then(|| config.options.clone());
        *self.config.write().unwrap() = Arc::new(config);
        *self.explicit_pending.lock().unwrap() = explicit;
        self.restart_dependency_index();
        self.inbox.wake();
    }
}

impl super::DaemonState {
    /// A new configuration replaces the catalog generation: no catalog built
    /// under the old one is published, and the next build (after a
    /// publication or an explicit rebuild) uses the new catalog options.
    fn restart_dependency_index(&self) {
        let enabled = self.config().dependency_options.is_some();
        let mut index = self.dependencies.lock().unwrap();
        index
            .cancel
            .store(true, std::sync::atomic::Ordering::Release);
        index.cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        index.generation += 1;
        index.catalog = None;
        index.warnings.clear();
        index.state = if enabled { "loading" } else { "disabled" };
    }
}
