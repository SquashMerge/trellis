//! Local search-model configuration and single-owner inference.
//!
//! Construction selects a backend but never reads model files. Jobs validate file
//! identities before reusing a resident snapshot; the service owns idle expiry.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use serde::Serialize;

use crate::search_embed::{
    Backend, EmbedError, Embedder, EmbeddingControl, ModelVerification, SearchEmbedder,
    SearchEmbedderFactory, backend_available, model_id_for_card_format, supported_model_name,
};

/// Opt-in diagnostics for a quiet search query; never reads the clock when disabled.
pub(crate) fn profiling_enabled() -> bool {
    tracing::enabled!(target: "trellis::search_profile", tracing::Level::DEBUG)
}

pub(crate) struct SearchPhaseTimer {
    phase: &'static str,
    started: Option<std::time::Instant>,
}

impl SearchPhaseTimer {
    pub(crate) fn start(phase: &'static str) -> Self {
        Self {
            phase,
            started: profiling_enabled().then(std::time::Instant::now),
        }
    }

    pub(crate) fn record(phase: &'static str, elapsed: std::time::Duration) {
        tracing::debug!(
            target: "trellis::search_profile",
            phase,
            elapsed_us = elapsed.as_micros(),
            "search phase"
        );
    }
}

impl Drop for SearchPhaseTimer {
    fn drop(&mut self) {
        if let Some(started) = self.started {
            Self::record(self.phase, started.elapsed());
        }
    }
}

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

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchUnavailableReason {
    ModelNotConfigured,
    ModelLoadFailed,
    VectorBudgetExceeded,
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
    pub unavailable_reason: Option<SearchUnavailableReason>,
}

/// Daemon-wide diagnostics, not query-pinned retrieval coverage.
/// A standalone registry without a search service has no selected backend or
/// configuration knowledge; its default diagnostics stay null.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchAvailability {
    pub backend: Option<Backend>,
    pub unavailable_reason: Option<SearchUnavailableReason>,
}

const MODEL_AVAILABLE_OR_UNTRIED: u8 = 0;
const MODEL_NOT_CONFIGURED: u8 = 1;
const MODEL_LOAD_FAILED: u8 = 2;

#[derive(Clone)]
pub struct SearchModelStatus {
    backend: Backend,
    ready: Arc<AtomicBool>,
    model_unavailable: Arc<AtomicU8>,
}

impl SearchModelStatus {
    pub(crate) fn backend(&self) -> Backend {
        self.backend
    }

    pub fn availability(&self) -> SearchAvailability {
        SearchAvailability {
            backend: Some(self.backend),
            unavailable_reason: self.unavailable_reason(),
        }
    }

    pub(crate) fn unavailable_reason(&self) -> Option<SearchUnavailableReason> {
        match self.model_unavailable.load(Ordering::Acquire) {
            MODEL_NOT_CONFIGURED => Some(SearchUnavailableReason::ModelNotConfigured),
            MODEL_LOAD_FAILED => Some(SearchUnavailableReason::ModelLoadFailed),
            _ => None,
        }
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

    /// A query owns its readiness decision independently of resident idle state.
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
            unavailable_reason: if ready {
                None
            } else {
                self.unavailable_reason()
            },
        }
    }
}

/// One mutable owner serializes jobs and owns the resident model.
/// Only the status handle is shared; the service owns idle-expiry policy.
pub struct SearchModel {
    config: SearchConfig,
    status: SearchModelStatus,
    verification: ModelVerification,
    factory: Option<Arc<dyn SearchEmbedderFactory>>,
    resident: Option<Box<dyn SearchEmbedder>>,
}

impl SearchModel {
    /// Explicit Metal refusal takes precedence over any model-path failure.
    pub fn new(config: SearchConfig) -> Result<Self, EmbedError> {
        let backend = select_backend(config.backend, backend_available)?;
        let unavailable = if config.model_path.is_none() {
            MODEL_NOT_CONFIGURED
        } else {
            MODEL_AVAILABLE_OR_UNTRIED
        };
        Ok(Self {
            config,
            verification: ModelVerification::default(),
            factory: None,
            resident: None,
            status: SearchModelStatus {
                backend,
                ready: Arc::new(AtomicBool::new(false)),
                model_unavailable: Arc::new(AtomicU8::new(unavailable)),
            },
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_embedder_for_tests(
        factory: Arc<dyn SearchEmbedderFactory>,
    ) -> Result<Self, EmbedError> {
        let mut model = Self::new(SearchConfig::from_values(None, Some("cpu"))?)?;
        model.factory = Some(factory);
        model
            .status
            .model_unavailable
            .store(MODEL_AVAILABLE_OR_UNTRIED, Ordering::Release);
        Ok(model)
    }

    pub fn backend(&self) -> Backend {
        self.status.backend
    }

    pub fn status_handle(&self) -> SearchModelStatus {
        self.status.clone()
    }

    pub(crate) fn drop_resident(&mut self) {
        self.resident = None;
    }

    pub(crate) fn has_resident(&self) -> bool {
        self.resident.is_some()
    }

    /// Cancellation and the optional query budget are checked across loading.
    /// The operation must pass the same control to embedding calls.
    pub fn with_job<T>(
        &mut self,
        control: &EmbeddingControl<'_>,
        operation: impl FnOnce(&dyn SearchEmbedder) -> Result<T, EmbedError>,
    ) -> Result<T, EmbedError> {
        let path = self.config.model_path.as_deref();
        let backend = self.status.backend;
        let verification = &mut self.verification;
        let factory = self.factory.as_ref();
        let operation_started = std::cell::Cell::new(false);
        let result = run_resident_job(
            &self.status.ready,
            &mut self.resident,
            control,
            verification,
            |verification| {
                if factory.is_some() {
                    return Ok(true);
                }
                let path = path.ok_or_else(|| {
                    EmbedError::ModelUnavailable("no local search model is configured".into())
                })?;
                let _phase = SearchPhaseTimer::start("model_verify_identity");
                verification.unchanged(path, control)
            },
            |verification| {
                if let Some(factory) = factory {
                    return factory.load(control);
                }
                let path = path.ok_or_else(|| {
                    EmbedError::ModelUnavailable("no local search model is configured".into())
                })?;
                let verified = {
                    let _phase = SearchPhaseTimer::start("model_verify_read");
                    verification.read(path, control)?
                };
                control.check()?;
                let _phase = SearchPhaseTimer::start("model_load");
                Ok(
                    Box::new(Embedder::from_verified(verified, backend, control)?)
                        as Box<dyn SearchEmbedder>,
                )
            },
            |model| {
                operation_started.set(true);
                self.status
                    .model_unavailable
                    .store(MODEL_AVAILABLE_OR_UNTRIED, Ordering::Release);
                operation(model.as_ref())
            },
        );
        if !operation_started.get()
            && (path.is_some() || factory.is_some())
            && matches!(
                &result,
                Err(EmbedError::ModelUnavailable(_) | EmbedError::Runtime(_))
            )
        {
            self.status
                .model_unavailable
                .store(MODEL_LOAD_FAILED, Ordering::Release);
        }
        result
    }
}

fn run_resident_job<M, T, V>(
    ready: &AtomicBool,
    resident: &mut Option<M>,
    control: &EmbeddingControl<'_>,
    verification: &mut V,
    unchanged: impl FnOnce(&mut V) -> Result<bool, EmbedError>,
    load: impl FnOnce(&mut V) -> Result<M, EmbedError>,
    operation: impl FnOnce(&M) -> Result<T, EmbedError>,
) -> Result<T, EmbedError> {
    ready.store(false, Ordering::Release);
    let _readiness = JobReadiness(ready);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        control.check()?;
        if resident.is_some() {
            match unchanged(verification) {
                Ok(true) => {}
                Ok(false) => *resident = None,
                Err(error) => {
                    if !matches!(error, EmbedError::Cancelled | EmbedError::BudgetExceeded) {
                        *resident = None;
                    }
                    return Err(error);
                }
            }
        }
        if resident.is_none() {
            let model = load(verification)?;
            control.check()?;
            *resident = Some(model);
        }
        ready.store(true, Ordering::Release);
        let result = operation(resident.as_ref().expect("model loaded"))?;
        control.check()?;
        Ok(result)
    }))
    .unwrap_or_else(|_| Err(EmbedError::Runtime("search model job panicked".into())));
    if matches!(&result, Err(EmbedError::Runtime(_))) {
        *resident = None;
    }
    result
}

struct JobReadiness<'a>(&'a AtomicBool);

impl Drop for JobReadiness<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
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

    struct CountingFactory {
        loads: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl SearchEmbedderFactory for CountingFactory {
        fn load(
            &self,
            control: &EmbeddingControl<'_>,
        ) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
            control.check()?;
            self.loads.fetch_add(1, Ordering::Relaxed);
            Ok(Box::new(TrackedModel(self.drops.clone())))
        }
    }

    impl SearchEmbedder for TrackedModel {
        fn embed(
            &self,
            _texts: &[String],
            _kind: crate::search_embed::InputKind,
            control: &EmbeddingControl<'_>,
        ) -> Result<Vec<crate::search_embed::QuantizedVector>, EmbedError> {
            control.check()?;
            Ok(Vec::new())
        }
    }

    #[test]
    fn successful_load_clears_failure_and_cancel_does_not_create_one() {
        struct RetryFactory(AtomicBool);
        impl SearchEmbedderFactory for RetryFactory {
            fn load(
                &self,
                _: &EmbeddingControl<'_>,
            ) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
                if self.0.swap(false, Ordering::AcqRel) {
                    Err(EmbedError::ModelUnavailable("fixture load failure".into()))
                } else {
                    Ok(Box::new(TrackedModel(Arc::new(AtomicUsize::new(0)))))
                }
            }
        }
        let mut owner =
            SearchModel::with_embedder_for_tests(Arc::new(RetryFactory(AtomicBool::new(true))))
                .unwrap();
        let status = owner.status_handle();
        let cancel = AtomicBool::new(true);
        let result: Result<(), EmbedError> =
            owner.with_job(&EmbeddingControl::new(&cancel), |_| unreachable!());
        assert!(matches!(result, Err(EmbedError::Cancelled)));
        assert_eq!(status.availability().unavailable_reason, None);
        cancel.store(false, Ordering::Release);
        let result: Result<(), EmbedError> =
            owner.with_job(&EmbeddingControl::new(&cancel), |_| unreachable!());
        assert!(matches!(result, Err(EmbedError::ModelUnavailable(_))));
        assert_eq!(
            status.availability().unavailable_reason,
            Some(SearchUnavailableReason::ModelLoadFailed)
        );
        owner
            .with_job(&EmbeddingControl::new(&cancel), |_| {
                assert_eq!(status.availability().unavailable_reason, None);
                Ok(())
            })
            .unwrap();
        owner.drop_resident();
        assert_eq!(status.availability().unavailable_reason, None);
    }

    #[test]
    fn completed_jobs_reuse_resident_until_explicit_release() {
        let loads = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut owner = SearchModel::with_embedder_for_tests(Arc::new(CountingFactory {
            loads: loads.clone(),
            drops: drops.clone(),
        }))
        .unwrap();
        let cancel = AtomicBool::new(false);
        for _ in 0..2 {
            owner
                .with_job(&EmbeddingControl::new(&cancel), |_| Ok(()))
                .unwrap();
            assert!(owner.has_resident());
            assert!(!owner.status.ready.load(Ordering::Acquire));
        }
        assert_eq!(loads.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        owner.drop_resident();
        assert!(!owner.has_resident());
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        owner
            .with_job(&EmbeddingControl::new(&cancel), |_| Ok(()))
            .unwrap();
        assert_eq!(loads.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn resident_errors_keep_control_failures_but_evict_runtime_or_panic() {
        for exit in ["budget", "cancel", "runtime", "panic"] {
            let drops = Arc::new(AtomicUsize::new(0));
            let ready = AtomicBool::new(false);
            let cancel = AtomicBool::new(false);
            let mut resident = Some(TrackedModel(drops.clone()));
            let result: Result<(), EmbedError> = run_resident_job(
                &ready,
                &mut resident,
                &EmbeddingControl::new(&cancel),
                &mut (),
                |_| Ok(true),
                |_| panic!("warm model reloaded"),
                |_| match exit {
                    "budget" => Err(EmbedError::BudgetExceeded),
                    "cancel" => Err(EmbedError::Cancelled),
                    "runtime" => Err(EmbedError::Runtime("fault".into())),
                    _ => panic!("inference fault"),
                },
            );
            assert!(result.is_err());
            assert_eq!(resident.is_some(), matches!(exit, "budget" | "cancel"));
            assert_eq!(
                drops.load(Ordering::Relaxed),
                usize::from(matches!(exit, "runtime" | "panic"))
            );
            assert!(!ready.load(Ordering::Acquire));
            assert!(!cancel.load(Ordering::Acquire));
        }
    }

    #[test]
    fn changed_or_unreadable_identity_drops_before_reload_or_return() {
        for fault in [false, true] {
            let ready = AtomicBool::new(false);
            let cancel = AtomicBool::new(false);
            let drops = Arc::new(AtomicUsize::new(0));
            let mut resident = Some(TrackedModel(drops.clone()));
            let result: Result<(), EmbedError> = run_resident_job(
                &ready,
                &mut resident,
                &EmbeddingControl::new(&cancel),
                &mut (),
                |_| {
                    if fault {
                        Err(EmbedError::ModelUnavailable("metadata fault".into()))
                    } else {
                        Ok(false)
                    }
                },
                |_| {
                    assert_eq!(drops.load(Ordering::Relaxed), 1);
                    Err(EmbedError::ModelUnavailable("reload fault".into()))
                },
                |_| panic!("stale model used"),
            );
            assert!(matches!(result, Err(EmbedError::ModelUnavailable(_))));
            assert!(resident.is_none());
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn loading_is_unavailable_and_loaded_job_reports_backend_and_coverage() {
        let owner =
            SearchModel::new(SearchConfig::from_values(None, Some("cpu")).unwrap()).unwrap();
        let status = owner.status_handle();
        let drops = Arc::new(AtomicUsize::new(0));
        let cancel = AtomicBool::new(false);
        let mut resident = None;
        run_resident_job(
            &status.ready,
            &mut resident,
            &EmbeddingControl::new(&cancel),
            &mut (),
            |_| panic!("cold load checked resident identity"),
            |_| {
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
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert!(resident.is_some());
        assert_eq!(
            status
                .snapshot("test-card-format.v1", 3, 3, LexicalState::Complete, true)
                .state,
            SearchIndexState::Unavailable
        );
    }

    #[test]
    fn job_exit_clears_readiness_and_keeps_only_valid_resident() {
        for exit in ["error", "panic", "cancel"] {
            let ready = AtomicBool::new(false);
            let cancel = AtomicBool::new(false);
            let drops = Arc::new(AtomicUsize::new(0));
            let mut resident = None;
            let result: Result<(), EmbedError> = run_resident_job(
                &ready,
                &mut resident,
                &EmbeddingControl::new(&cancel),
                &mut (),
                |_| panic!("cold load checked resident identity"),
                |_| Ok(TrackedModel(drops.clone())),
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
            assert_eq!(resident.is_some(), exit == "cancel");
            assert_eq!(drops.load(Ordering::Relaxed), usize::from(exit != "cancel"));
        }
    }

    #[test]
    fn budget_exhaustion_during_load_releases_weights_without_poisoning_cancel() {
        use crate::search_embed::QueryBudget;
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let expired = Arc::new(AtomicBool::new(false));
        let clock_expired = expired.clone();
        let budget = QueryBudget::new(
            start + Duration::from_secs(1),
            Arc::new(move || {
                if clock_expired.load(Ordering::Acquire) {
                    start + Duration::from_secs(1)
                } else {
                    start
                }
            }),
        );
        let ready = AtomicBool::new(false);
        let cancel = AtomicBool::new(false);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut resident = None;
        let result: Result<(), EmbedError> = run_resident_job(
            &ready,
            &mut resident,
            &EmbeddingControl::with_budget(&cancel, &budget),
            &mut (),
            |_| panic!("cold load checked resident identity"),
            |_| {
                expired.store(true, Ordering::Release);
                Ok(TrackedModel(drops.clone()))
            },
            |_| panic!("expired budget invoked operation"),
        );
        assert!(resident.is_none());
        assert!(matches!(result, Err(EmbedError::BudgetExceeded)));
        assert!(!ready.load(Ordering::Acquire));
        assert!(!cancel.load(Ordering::Acquire));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancellation_before_or_during_load_never_invokes_operation() {
        let ready = AtomicBool::new(false);
        let cancel = AtomicBool::new(true);
        let mut resident = None;
        let result: Result<(), EmbedError> = run_resident_job(
            &ready,
            &mut resident,
            &EmbeddingControl::new(&cancel),
            &mut (),
            |_| panic!("cold load checked resident identity"),
            |_| -> Result<TrackedModel, EmbedError> { panic!("cancelled before loading") },
            |_| panic!("cancelled operation"),
        );
        assert!(matches!(result, Err(EmbedError::Cancelled)));
        cancel.store(false, Ordering::Release);
        let drops = Arc::new(AtomicUsize::new(0));
        let mut resident = None;
        let result: Result<(), EmbedError> = run_resident_job(
            &ready,
            &mut resident,
            &EmbeddingControl::new(&cancel),
            &mut (),
            |_| panic!("cold load checked resident identity"),
            |_| {
                cancel.store(true, Ordering::Release);
                Ok(TrackedModel(drops.clone()))
            },
            |_| panic!("cancelled during loading"),
        );
        assert!(matches!(result, Err(EmbedError::Cancelled)));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert!(!ready.load(Ordering::Acquire));
        cancel.store(false, Ordering::Release);
        let mut resident = None;
        let result: Result<(), EmbedError> = run_resident_job(
            &ready,
            &mut resident,
            &EmbeddingControl::new(&cancel),
            &mut (),
            |_| panic!("cold load checked resident identity"),
            |_| {
                cancel.store(true, Ordering::Release);
                Err::<TrackedModel, _>(EmbedError::ModelUnavailable(
                    "load failed while cancelled".into(),
                ))
            },
            |_| panic!("cancelled failed load"),
        );
        assert!(matches!(result, Err(EmbedError::ModelUnavailable(_))));
    }
}
