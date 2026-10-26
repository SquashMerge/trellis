//! Local search-model configuration and single-owner, job-scoped inference.
//!
//! Construction selects a backend but never reads model files. Jobs verify each
//! model file identity once, load the snapshot, and release all weights on exit.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::search_embed::{
    Backend, EmbedError, Embedder, ModelVerification, SearchEmbedder, SearchEmbedderFactory,
    backend_available, model_id_for_card_format, supported_model_name,
};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackendPreference {
    #[default]
    Auto,
    Cpu,
    Metal,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SearchConfig {
    model_path: Option<PathBuf>,
    backend: BackendPreference,
}

impl SearchConfig {
    /// `None` selects automatic backend choice. Model paths are local directories;
    /// neither configuration nor loading downloads anything.
    pub fn from_values(
        model_path: Option<PathBuf>,
        backend: Option<&str>,
    ) -> Result<Self, EmbedError> {
        let backend = match backend.unwrap_or("auto") {
            "auto" => BackendPreference::Auto,
            "cpu" => BackendPreference::Cpu,
            "metal" => BackendPreference::Metal,
            value => {
                return Err(EmbedError::InvalidInput(format!(
                    "search backend must be auto, cpu or metal, not {value:?}"
                )));
            }
        };
        Ok(Self {
            model_path,
            backend,
        })
    }

    pub fn model_path(&self) -> Option<&Path> {
        self.model_path.as_deref()
    }

    pub fn backend_preference(&self) -> BackendPreference {
        self.backend
    }
}

fn select_backend(
    preference: BackendPreference,
    mut available: impl FnMut(Backend) -> Result<(), EmbedError>,
) -> Result<Backend, EmbedError> {
    match preference {
        BackendPreference::Cpu => Ok(Backend::Cpu),
        BackendPreference::Metal => {
            available(Backend::Metal)?;
            Ok(Backend::Metal)
        }
        BackendPreference::Auto => {
            if cfg!(all(target_os = "macos", feature = "metal"))
                && available(Backend::Metal).is_ok()
            {
                Ok(Backend::Metal)
            } else {
                Ok(Backend::Cpu)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SearchIndexState {
    Complete,
    Building,
    Partial,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LexicalState {
    Complete,
    Unavailable,
}

/// Retrieval metadata only, independent of native-evidence completeness.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchIndexStatus {
    pub state: SearchIndexState,
    pub model_id: Option<String>,
    pub model_name: Option<String>,
    pub backend: Option<Backend>,
    pub declarations: u64,
    pub embedded: u64,
    pub lexical: LexicalState,
}

#[derive(Clone)]
pub struct SearchModelStatus {
    backend: Backend,
    ready: Arc<AtomicBool>,
}

impl SearchModelStatus {
    pub(crate) fn backend(&self) -> Backend {
        self.backend
    }

    /// Counts and `building` come from the caller's coherent owner-published
    /// index/cache snapshot. A loaded query model alone is not a background job.
    /// The caller explicitly names its reviewed card serializer. There is no
    /// default format. This accessor opens no index and creates no evidence.
    pub fn snapshot(
        &self,
        card_format: &str,
        declarations: u64,
        embedded: u64,
        lexical: LexicalState,
        building: bool,
    ) -> SearchIndexStatus {
        self.snapshot_with_readiness(
            self.ready.load(Ordering::Acquire),
            card_format,
            declarations,
            embedded,
            lexical,
            building,
        )
    }

    /// A query owns its readiness decision, even after its job drops weights.
    /// Do not resample another job's live flag when projecting that query.
    pub(crate) fn snapshot_with_readiness(
        &self,
        ready: bool,
        card_format: &str,
        declarations: u64,
        embedded: u64,
        lexical: LexicalState,
        building: bool,
    ) -> SearchIndexStatus {
        SearchIndexStatus {
            state: if !ready {
                SearchIndexState::Unavailable
            } else if embedded == declarations {
                SearchIndexState::Complete
            } else if building {
                SearchIndexState::Building
            } else {
                SearchIndexState::Partial
            },
            model_id: ready.then(|| model_id_for_card_format(card_format)),
            model_name: ready.then(|| supported_model_name().to_owned()),
            backend: Some(self.backend),
            declarations,
            embedded: if ready { embedded } else { 0 },
            lexical,
        }
    }
}

/// One mutable owner serializes jobs. Only the status handle is shared; no
/// runtime or weights stay resident between operations.
pub struct SearchModel {
    config: SearchConfig,
    status: SearchModelStatus,
    verification: ModelVerification,
    factory: Option<Arc<dyn SearchEmbedderFactory>>,
}

impl SearchModel {
    /// Explicit Metal refusal takes precedence over any model-path failure.
    pub fn new(config: SearchConfig) -> Result<Self, EmbedError> {
        let backend = select_backend(config.backend, backend_available)?;
        Ok(Self {
            config,
            verification: ModelVerification::default(),
            factory: None,
            status: SearchModelStatus {
                backend,
                ready: Arc::new(AtomicBool::new(false)),
            },
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_embedder_for_tests(
        factory: Arc<dyn SearchEmbedderFactory>,
    ) -> Result<Self, EmbedError> {
        let mut model = Self::new(SearchConfig::from_values(None, Some("cpu"))?)?;
        model.factory = Some(factory);
        Ok(model)
    }

    pub fn backend(&self) -> Backend {
        self.status.backend
    }

    pub fn status_handle(&self) -> SearchModelStatus {
        self.status.clone()
    }

    /// Cancellation is checked before/after loading and before returning a result.
    /// The operation must pass the same flag to embedding calls for batch checks.
    pub fn with_job<T>(
        &mut self,
        cancel: &AtomicBool,
        operation: impl FnOnce(&dyn SearchEmbedder) -> Result<T, EmbedError>,
    ) -> Result<T, EmbedError> {
        let path = self.config.model_path.as_deref();
        let backend = self.status.backend;
        let verification = &mut self.verification;
        let factory = self.factory.as_ref();
        run_job(
            &self.status.ready,
            cancel,
            || {
                if let Some(factory) = factory {
                    return factory.load(cancel);
                }
                let path = path.ok_or_else(|| {
                    EmbedError::ModelUnavailable("no local search model is configured".into())
                })?;
                let verified = verification.read(path)?;
                check_cancel(cancel)?;
                Ok(Box::new(Embedder::from_verified(verified, backend)?)
                    as Box<dyn SearchEmbedder>)
            },
            |model| operation(model.as_ref()),
        )
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), EmbedError> {
    if cancel.load(Ordering::Acquire) {
        Err(EmbedError::Cancelled)
    } else {
        Ok(())
    }
}

struct JobReadiness<'a>(&'a AtomicBool);

impl Drop for JobReadiness<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

fn run_job<M, T>(
    ready: &AtomicBool,
    cancel: &AtomicBool,
    load: impl FnOnce() -> Result<M, EmbedError>,
    operation: impl FnOnce(&M) -> Result<T, EmbedError>,
) -> Result<T, EmbedError> {
    ready.store(false, Ordering::Release);
    let _readiness = JobReadiness(ready);
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_cancel(cancel)?;
        let model = load();
        check_cancel(cancel)?;
        let model = model?;
        ready.store(true, Ordering::Release);
        let result = operation(&model)?;
        check_cancel(cancel)?;
        Ok(result)
    }))
    .unwrap_or_else(|_| Err(EmbedError::Runtime("search model job panicked".into())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn selection_falls_back_only_in_automatic_mode() {
        let unavailable = |_| Err(EmbedError::ModelUnavailable("no Metal device".into()));
        assert_eq!(
            select_backend(BackendPreference::Auto, unavailable).unwrap(),
            Backend::Cpu
        );
        assert!(matches!(
            select_backend(BackendPreference::Metal, unavailable),
            Err(EmbedError::ModelUnavailable(_))
        ));
        assert_eq!(
            select_backend(BackendPreference::Auto, |_| Ok(())).unwrap(),
            if cfg!(all(target_os = "macos", feature = "metal")) {
                Backend::Metal
            } else {
                Backend::Cpu
            }
        );
        assert_eq!(
            select_backend(BackendPreference::Cpu, |_| panic!("CPU requires no probe")).unwrap(),
            Backend::Cpu
        );
    }

    struct TrackedModel(Arc<AtomicUsize>);
    impl Drop for TrackedModel {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn loading_is_unavailable_and_loaded_job_reports_backend_and_coverage() {
        let owner =
            SearchModel::new(SearchConfig::from_values(None, Some("cpu")).unwrap()).unwrap();
        let status = owner.status_handle();
        let drops = Arc::new(AtomicUsize::new(0));
        let cancel = AtomicBool::new(false);
        run_job(
            &status.ready,
            &cancel,
            || {
                assert_eq!(
                    status
                        .snapshot("test-card-format.v1", 3, 0, LexicalState::Complete, true)
                        .state,
                    SearchIndexState::Unavailable
                );
                Ok(TrackedModel(drops.clone()))
            },
            |_| {
                let building =
                    status.snapshot("test-card-format.v1", 3, 2, LexicalState::Complete, true);
                assert_eq!(building.state, SearchIndexState::Building);
                assert_eq!(
                    status
                        .snapshot("test-card-format.v1", 3, 2, LexicalState::Complete, false)
                        .state,
                    SearchIndexState::Partial
                );
                assert_eq!(
                    building.model_id,
                    Some(model_id_for_card_format("test-card-format.v1"))
                );
                assert_eq!(building.model_name.as_deref(), Some(supported_model_name()));
                assert_eq!(building.backend, Some(Backend::Cpu));
                assert_eq!(
                    status
                        .snapshot("test-card-format.v1", 3, 3, LexicalState::Complete, true)
                        .state,
                    SearchIndexState::Complete
                );
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(
            status
                .snapshot("test-card-format.v1", 3, 3, LexicalState::Complete, true)
                .state,
            SearchIndexState::Unavailable
        );
    }

    #[test]
    fn every_job_exit_releases_model_and_readiness() {
        for exit in ["error", "panic", "cancel"] {
            let ready = AtomicBool::new(false);
            let cancel = AtomicBool::new(false);
            let drops = Arc::new(AtomicUsize::new(0));
            let result: Result<(), EmbedError> = run_job(
                &ready,
                &cancel,
                || Ok(TrackedModel(drops.clone())),
                |_| match exit {
                    "error" => Err(EmbedError::Runtime("operation failed".into())),
                    "panic" => panic!("operation panicked"),
                    _ => {
                        cancel.store(true, Ordering::Release);
                        Ok(())
                    }
                },
            );
            assert!(result.is_err());
            assert!(!ready.load(Ordering::Acquire));
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn cancellation_before_or_during_load_never_invokes_operation() {
        let ready = AtomicBool::new(false);
        let cancel = AtomicBool::new(true);
        let result: Result<(), EmbedError> = run_job(
            &ready,
            &cancel,
            || -> Result<(), EmbedError> { panic!("cancelled before loading") },
            |_| panic!("cancelled operation"),
        );
        assert!(matches!(result, Err(EmbedError::Cancelled)));
        cancel.store(false, Ordering::Release);
        let drops = Arc::new(AtomicUsize::new(0));
        let result: Result<(), EmbedError> = run_job(
            &ready,
            &cancel,
            || {
                cancel.store(true, Ordering::Release);
                Ok(TrackedModel(drops.clone()))
            },
            |_| panic!("cancelled during loading"),
        );
        assert!(matches!(result, Err(EmbedError::Cancelled)));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(!ready.load(Ordering::Acquire));
        cancel.store(false, Ordering::Release);
        let result: Result<(), EmbedError> = run_job(
            &ready,
            &cancel,
            || {
                cancel.store(true, Ordering::Release);
                Err::<(), _>(EmbedError::ModelUnavailable(
                    "load failed while cancelled".into(),
                ))
            },
            |_| panic!("cancelled failed load"),
        );
        assert!(matches!(result, Err(EmbedError::Cancelled)));
    }
}
