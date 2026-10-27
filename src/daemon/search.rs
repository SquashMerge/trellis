//! Daemon-wide local model and vector-cache ownership, independent of checkouts.
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
};

use crate::{
    search_card::CardFormat,
    search_embed::{
        Backend, DIMENSIONS, EmbedError, EmbeddingControl, InputKind, QueryBudget, SearchEmbedder,
    },
    search_model::{
        LexicalState, SearchConfig, SearchIndexState, SearchIndexStatus, SearchModel,
        SearchModelStatus, SearchUnavailableReason,
    },
    search_vectors::{
        VectorCacheReader, VectorCacheReport, VectorCacheWriter, VectorSnapshot, is_corrupt_cache,
        report_existing,
    },
    store::topology::TopologyRoots,
};
use anyhow::Result;

const MODEL_IDLE: std::time::Duration = std::time::Duration::from_secs(600);
type ModelClock = Arc<dyn Fn() -> std::time::Instant + Send + Sync>;

/// One call's model decision and query vector. Readiness is captured when the
/// query owns the model job, not sampled from a later fill/query job.
pub struct SearchQuery {
    pub vector: Option<Vec<i8>>,
    pub budget: Option<QueryBudget>,
    status: SearchIndexStatus,
}

impl SearchQuery {
    /// Standalone/test registries may have no daemon search service or selected
    /// backend. A configured service instead preserves its selected backend.
    pub fn unavailable(backend: Option<Backend>) -> Self {
        Self {
            vector: None,
            budget: None,
            status: SearchIndexStatus {
                state: SearchIndexState::Unavailable,
                model_id: None,
                model_name: None,
                backend,
                declarations: 0,
                embedded: 0,
                lexical: LexicalState::Complete,
                unavailable_reason: None,
            },
        }
    }

    pub fn budget_exceeded(backend: Option<Backend>, budget: QueryBudget) -> Self {
        let mut query = Self::unavailable(backend);
        query.budget = Some(budget);
        query.mark_budget_exceeded();
        query
    }

    pub fn mark_budget_exceeded(&mut self) {
        let backend = self.status.backend;
        self.vector = None;
        self.status = Self::unavailable(backend).status;
        self.status.unavailable_reason = Some(SearchUnavailableReason::VectorBudgetExceeded);
    }

    pub fn budget(&self) -> Option<&QueryBudget> {
        self.budget.as_ref()
    }

    pub fn model_id(&self) -> Option<&str> {
        self.status.model_id.as_deref()
    }

    pub fn status(&self, declarations: u64, embedded: u64, building: bool) -> SearchIndexStatus {
        let mut status = self.status.clone();
        status.declarations = declarations;
        if status.model_id.is_some() {
            status.embedded = embedded;
            status.state = if embedded == declarations {
                SearchIndexState::Complete
            } else if building {
                SearchIndexState::Building
            } else {
                SearchIndexState::Partial
            };
        }
        status
    }
}

#[derive(Debug, Default, Eq, PartialEq)]
enum ModelPhase {
    #[default]
    Idle,
    Loading,
    Ready,
    Closing,
}

struct QueryRequest {
    id: u64,
    text: String,
    cancel: Arc<AtomicBool>,
    deadline: std::time::Instant,
    budget: Option<QueryBudget>,
    reply: mpsc::Sender<Result<Option<Vec<i8>>, EmbedError>>,
}

#[derive(Default)]
struct Admission {
    next_id: u64,
    phase: ModelPhase,
    queries: VecDeque<QueryRequest>,
    idle_until: Option<std::time::Instant>,
    head_users: usize,
}

struct QueuedQuery<'a> {
    service: &'a SearchService,
    id: u64,
}
impl Drop for QueuedQuery<'_> {
    fn drop(&mut self) {
        let mut admission = self
            .service
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        admission.queries.retain(|request| request.id != self.id);
        self.service.model_changed.notify_all();
        drop(admission);
        self.service.wake_maintenance();
    }
}

/// Publish idle residency only after the job releases the model lock.
/// Surviving FIFO tickets carry across admission close/reopen, including unwind.
struct ModelJob<'a>(&'a SearchService);
impl Drop for ModelJob<'_> {
    fn drop(&mut self) {
        let resident = self
            .0
            .model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .has_resident();
        {
            let mut admission = self
                .0
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            admission.phase = ModelPhase::Idle;
            admission.idle_until = resident.then(|| (self.0.model_clock)() + MODEL_IDLE);
            self.0.model_changed.notify_all();
        }
        self.0.wake_maintenance();
    }
}

pub(super) struct HeadUse<'a>(&'a SearchService);
impl Drop for HeadUse<'_> {
    fn drop(&mut self) {
        let mut admission = self
            .0
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        admission.head_users -= 1;
        if admission.phase == ModelPhase::Idle {
            admission.idle_until = Some((self.0.model_clock)() + MODEL_IDLE);
        }
        self.0.model_changed.notify_all();
        drop(admission);
        self.0.wake_maintenance();
    }
}

pub struct SearchService {
    model: Mutex<SearchModel>,
    admission: Mutex<Admission>,
    model_changed: Condvar,
    cache_epoch: AtomicU64,
    status: SearchModelStatus,
    vectors: Mutex<Option<VectorCacheWriter>>,
    roots: TopologyRoots,
    model_configured: bool,
    model_clock: ModelClock,
    maintenance: Mutex<Option<Weak<super::search_fill::Core>>>,
}

impl SearchService {
    pub fn from_environment(roots: TopologyRoots) -> Result<Self> {
        let path = std::env::var_os("TRELLIS_SEARCH_MODEL").map(PathBuf::from);
        let backend =
            std::env::var("TRELLIS_SEARCH_BACKEND")
                .map(Some)
                .or_else(|error| match error {
                    std::env::VarError::NotPresent => Ok(None),
                    std::env::VarError::NotUnicode(_) => Err(EmbedError::InvalidInput(
                        "TRELLIS_SEARCH_BACKEND must be UTF-8".into(),
                    )),
                })?;
        Self::new(roots, SearchConfig::from_values(path, backend.as_deref())?)
    }

    pub fn new(roots: TopologyRoots, config: SearchConfig) -> Result<Self> {
        let model_configured = config.model_path().is_some();
        Self::from_model(roots, SearchModel::new(config)?, model_configured)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_embedder_for_tests(
        roots: TopologyRoots,
        factory: Arc<dyn crate::search_embed::SearchEmbedderFactory>,
    ) -> Result<Self> {
        Self::from_model(roots, SearchModel::with_embedder_for_tests(factory)?, true)
    }

    fn from_model(
        roots: TopologyRoots,
        model: SearchModel,
        model_configured: bool,
    ) -> Result<Self> {
        let status = model.status_handle();
        Ok(Self {
            model: Mutex::new(model),
            admission: Mutex::new(Admission::default()),
            model_changed: Condvar::new(),
            cache_epoch: AtomicU64::new(0),
            status,
            vectors: Mutex::new(None),
            roots,
            model_configured,
            model_clock: Arc::new(std::time::Instant::now),
            maintenance: Mutex::new(None),
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_for_queued_query_for_tests(&self) {
        let deadline = std::time::Instant::now() + crate::testing::HANG_BACKSTOP;
        let mut admission = self.admission.lock().unwrap();
        while admission.queries.is_empty() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "query did not enter the owner queue");
            admission = self
                .model_changed
                .wait_timeout(admission, remaining)
                .unwrap()
                .0;
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn set_model_clock_for_tests(&mut self, clock: ModelClock) {
        self.model_clock = clock;
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn wake_model_clock_for_tests(&self) {
        self.wake_maintenance();
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn wait_for_model_eviction_for_tests(&self) {
        let deadline = std::time::Instant::now() + crate::testing::HANG_BACKSTOP;
        let mut admission = self.admission.lock().unwrap();
        while admission.phase != ModelPhase::Idle || admission.idle_until.is_some() {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(!remaining.is_zero(), "resident model was not evicted");
            admission = self
                .model_changed
                .wait_timeout(admission, remaining)
                .unwrap()
                .0;
        }
    }

    pub fn model_configured(&self) -> bool {
        self.model_configured
    }

    pub fn status_handle(&self) -> SearchModelStatus {
        self.status.clone()
    }

    /// Call before acquiring any checkout/cache snapshot. Ready owners service
    /// queued queries between document batches; only loading is lexical-only.
    /// The shared flag is the caller's original deadline/cancellation flag.
    pub fn query_embedding(
        &self,
        query: &str,
        cancel: &Arc<AtomicBool>,
        deadline: std::time::Instant,
        budget: Option<QueryBudget>,
    ) -> Result<SearchQuery, EmbedError> {
        let _phase = crate::search_model::SearchPhaseTimer::start("query_model_job");
        query_check(cancel, deadline, budget.as_ref())?;
        let (id, receiver) = {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if admission.phase == ModelPhase::Loading {
                let mut query = SearchQuery::unavailable(Some(self.status.backend()));
                query.budget = budget;
                return Ok(query);
            }
            let (reply, receiver) = mpsc::channel();
            let id = admission.next_id;
            admission.next_id += 1;
            admission.queries.push_back(QueryRequest {
                id,
                text: query.to_owned(),
                cancel: cancel.clone(),
                deadline,
                budget: budget.clone(),
                reply,
            });
            self.model_changed.notify_all();
            (id, receiver)
        };
        let _request = QueuedQuery { service: self, id };
        let vector = loop {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match receiver.try_recv() {
                Ok(result) => break result?,
                Err(mpsc::TryRecvError::Disconnected) => break None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
            let check = query_check(cancel, deadline, budget.as_ref());
            if let Err(error) = check {
                if let Some(position) = admission
                    .queries
                    .iter()
                    .position(|request| request.id == id)
                {
                    // Removing a ticket under admission proves it cannot start.
                    admission.queries.remove(position);
                    self.model_changed.notify_all();
                    return Err(error);
                }
                // Already running: wait for the owner's cooperative unwind ACK.
                // Never cancel its fill job or use timeout as proof of a stop.
                drop(
                    self.model_changed
                        .wait(admission)
                        .unwrap_or_else(|error| error.into_inner()),
                );
                continue;
            }
            if admission.phase == ModelPhase::Idle
                && admission
                    .queries
                    .front()
                    .is_some_and(|request| request.id == id)
            {
                admission.queries.pop_front();
                admission.phase = ModelPhase::Loading;
                admission.idle_until = None;
                drop(admission);
                let control = query_control(cancel, budget.as_ref());
                let _job = ModelJob(self);
                let mut owner = self.model.lock().unwrap_or_else(|error| error.into_inner());
                let result = owner.with_job(&control, |model| {
                    self.publish_ready();
                    let result = embed_query(model, query, &control);
                    // Direct owners run only their own query. Sibling tickets
                    // reopen admission in FIFO order and reuse resident weights.
                    self.admission
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .phase = ModelPhase::Closing;
                    self.model_changed.notify_all();
                    result
                });
                break query_result(result)?;
            }
            let now = budget
                .as_ref()
                .map_or_else(std::time::Instant::now, QueryBudget::now);
            let remaining = deadline.saturating_duration_since(now);
            let remaining = budget
                .as_ref()
                .map_or(remaining, |budget| remaining.min(budget.remaining()));
            drop(
                self.model_changed
                    .wait_timeout(admission, remaining)
                    .unwrap_or_else(|error| error.into_inner()),
            );
        };
        query_check(cancel, deadline, budget.as_ref())?;
        Ok(SearchQuery {
            status: self.status.snapshot_with_readiness(
                vector.is_some(),
                CardFormat::V1.as_str(),
                0,
                0,
                LexicalState::Complete,
                false,
            ),
            vector,
            budget,
        })
    }

    pub(super) fn set_maintenance(&self, core: Weak<super::search_fill::Core>) {
        *self
            .maintenance
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(core);
    }

    fn wake_maintenance(&self) {
        let core = self
            .maintenance
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(core) = core {
            core.wake();
        }
    }

    pub(super) fn idle_remaining(&self) -> Option<std::time::Duration> {
        let admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if admission.phase != ModelPhase::Idle
            || !admission.queries.is_empty()
            || admission.head_users != 0
        {
            return None;
        }
        admission
            .idle_until
            .map(|until| until.saturating_duration_since((self.model_clock)()))
    }

    /// The fill driver calls this while its inbox proves no pending fill work.
    /// Only claim admission here; never await the model while holding that inbox.
    pub(super) fn begin_idle_expiry(&self) -> bool {
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if admission.phase != ModelPhase::Idle
            || admission.head_users != 0
            || !admission.queries.is_empty()
            || !admission
                .idle_until
                .is_some_and(|until| (self.model_clock)() >= until)
        {
            return false;
        }
        admission.phase = ModelPhase::Closing;
        admission.idle_until = None;
        true
    }

    pub(super) fn finish_idle_expiry(&self) {
        self.model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .drop_resident();
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        admission.phase = ModelPhase::Idle;
        self.model_changed.notify_all();
    }

    fn publish_ready(&self) {
        self.admission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase = ModelPhase::Ready;
        self.model_changed.notify_all();
        let core = self
            .maintenance
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(core) = core {
            core.model_ready();
        }
    }

    pub(super) fn begin_head_use(&self) -> Option<HeadUse<'_>> {
        let mut admission = self
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if admission.phase == ModelPhase::Closing {
            return None;
        }
        admission.head_users += 1;
        admission.idle_until = None;
        Some(HeadUse(self))
    }

    pub(super) fn publish_head_vectors(
        &self,
        model_id: &str,
        vectors: &[(String, Vec<i8>)],
    ) -> Result<()> {
        let core = self
            .maintenance
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .and_then(Weak::upgrade);
        if let Some(core) = core {
            core.committed(model_id, self.cache_epoch(), vectors)?;
        }
        Ok(())
    }

    /// No store/registry/cache lock may be held by the caller at this boundary.
    pub(crate) fn serve_queries(&self, model: &dyn SearchEmbedder) {
        loop {
            let request = self
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .queries
                .pop_front();
            let Some(request) = request else { return };
            let control = query_control(&request.cancel, request.budget.as_ref());
            let result = query_check(&request.cancel, request.deadline, request.budget.as_ref())
                .and_then(|()| query_result(embed_query(model, &request.text, &control)));
            // Pair reply publication with the waiter's mutex to avoid a lost
            // completion notification between checking and waiting.
            let _admission = self
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let _ = request.reply.send(result);
            self.model_changed.notify_all();
        }
    }

    fn finish_queries(&self, model: &dyn SearchEmbedder) {
        loop {
            self.serve_queries(model);
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if admission.queries.is_empty() {
                // Idle is published only after the job releases its model lock.
                // Loaded weights remain resident until idle eviction.
                admission.phase = ModelPhase::Closing;
                return;
            }
        }
    }

    /// All per-call counts and ranking vectors must use this one read snapshot.
    /// Missing cache is ordinary absence; malformed vectors are misses through
    /// VectorSnapshot::get. Actual storage faults and contention remain typed.
    /// This path never takes the model lock or creates the optional cache.
    /// Recognized corruption releases the cached writer after the reader drops.
    pub fn with_vector_snapshot<T>(
        &self,
        operation: impl FnOnce(Option<&VectorSnapshot<'_>>) -> Result<T>,
    ) -> Result<T> {
        self.with_vector_snapshot_budget(None, operation)
    }

    pub fn with_vector_snapshot_budget<T>(
        &self,
        budget: Option<&QueryBudget>,
        operation: impl FnOnce(Option<&VectorSnapshot<'_>>) -> Result<T>,
    ) -> Result<T> {
        let result = (|| {
            if let Some(budget) = budget {
                budget.check()?;
            }
            let phase = crate::search_model::SearchPhaseTimer::start("vector_snapshot_open");
            let reader = match budget {
                Some(budget) => {
                    VectorCacheReader::open_existing_with_budget(&self.roots.vectors_db(), budget)?
                }
                None => VectorCacheReader::open_existing(&self.roots.vectors_db())?,
            };
            let Some(mut reader) = reader else {
                if let Some(budget) = budget {
                    budget.check()?;
                }
                drop(phase);
                let result = operation(None);
                return match result {
                    Ok(value) => {
                        if let Some(budget) = budget {
                            budget.check()?;
                        }
                        Ok(value)
                    }
                    Err(error) => Err(error),
                };
            };
            let snapshot = match budget {
                Some(budget) => reader.snapshot_with_budget(budget)?,
                None => reader.snapshot()?,
            };
            // Errors already returned by open/snapshot stay typed faults, even
            // when their completion coincides with the optional cutoff.
            if let Some(budget) = budget {
                budget.check()?;
            }
            drop(phase);
            let result = operation(Some(&snapshot));
            match result {
                Ok(value) => {
                    if let Some(budget) = budget {
                        budget.check()?;
                    }
                    Ok(value)
                }
                Err(error) => Err(error),
            }
        })();
        self.invalidate_corrupt_cache(&result);
        result
    }

    pub fn with_model_job<T>(
        &self,
        cancel: &AtomicBool,
        operation: impl FnOnce(&dyn SearchEmbedder) -> Result<T, EmbedError>,
    ) -> Result<T, EmbedError> {
        {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            while admission.phase != ModelPhase::Idle || !admission.queries.is_empty() {
                admission = self
                    .model_changed
                    .wait(admission)
                    .unwrap_or_else(|error| error.into_inner());
            }
            admission.phase = ModelPhase::Loading;
            admission.idle_until = None;
        }
        let _job = ModelJob(self);
        let mut owner = self.model.lock().unwrap_or_else(|error| error.into_inner());
        owner.with_job(&EmbeddingControl::new(cancel), |model| {
            self.publish_ready();
            let result = operation(model);
            self.finish_queries(model);
            result
        })
    }

    pub(crate) fn cache_epoch(&self) -> u64 {
        self.cache_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn fill_reader(&self) -> Result<Option<VectorCacheReader>> {
        VectorCacheReader::open_existing(&self.roots.vectors_db())
    }

    pub(crate) fn invalidate_corrupt_cache<T>(&self, result: &Result<T>) {
        if result.as_ref().is_err_and(is_corrupt_cache) {
            *self
                .vectors
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
            self.cache_epoch.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// The sole daemon writer is opened lazily; no checkout index is involved.
    pub fn with_vector_writer<T>(
        &self,
        operation: impl FnOnce(&mut VectorCacheWriter) -> Result<T>,
    ) -> Result<T> {
        let mut writer = self
            .vectors
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if writer.is_none() {
            self.roots.prepare_vectors()?;
            *writer = Some(VectorCacheWriter::open(&self.roots.vectors_db())?);
            self.cache_epoch.fetch_add(1, Ordering::AcqRel);
        }
        let before = writer.as_ref().unwrap().eviction_epoch();
        let result = operation(writer.as_mut().expect("vector writer initialized"));
        if writer.as_ref().unwrap().eviction_epoch() != before {
            self.cache_epoch.fetch_add(1, Ordering::AcqRel);
        }
        if result.as_ref().is_err_and(is_corrupt_cache) {
            // Reopen/recreate on the next job, not within a failed operation.
            *writer = None;
            self.cache_epoch.fetch_add(1, Ordering::AcqRel);
        }
        result
    }

    /// One read snapshot for a fill batch. Absence and decode-invalid vectors
    /// are misses; reads neither create a cache nor refresh its LRU ages.
    pub fn missing_vectors(&self, model_id: &str, card_hashes: &[&str]) -> Result<Vec<bool>> {
        let result = (|| {
            let Some(mut reader) = VectorCacheReader::open_existing(&self.roots.vectors_db())?
            else {
                return Ok(vec![true; card_hashes.len()]);
            };
            let snapshot = reader.snapshot()?;
            card_hashes
                .iter()
                .map(|card_hash| {
                    snapshot
                        .get(model_id, card_hash)
                        .map(|vector| vector.is_none_or(|vector| vector.len() != DIMENSIONS))
                })
                .collect()
        })();
        // Drop every reader before releasing a corrupt cached writer, so the
        // next fill job can safely reopen and recreate the disposable cache.
        self.invalidate_corrupt_cache(&result);
        result
    }

    pub fn gc_report(&self) -> Result<crate::store::topology::GcReport> {
        self.roots.gc_report()
    }

    /// Reporting does not acquire the writer lock or create an optional cache.
    pub fn vector_report(&self) -> Result<VectorCacheReport> {
        report_existing(&self.roots.vectors_db())
    }
}

fn query_control<'a>(
    cancel: &'a AtomicBool,
    budget: Option<&'a QueryBudget>,
) -> EmbeddingControl<'a> {
    match budget {
        Some(budget) => EmbeddingControl::with_budget(cancel, budget),
        None => EmbeddingControl::new(cancel),
    }
}

fn query_check(
    cancel: &AtomicBool,
    deadline: std::time::Instant,
    budget: Option<&QueryBudget>,
) -> Result<(), EmbedError> {
    if budget.map_or_else(std::time::Instant::now, QueryBudget::now) >= deadline {
        cancel.store(true, Ordering::Release);
    }
    query_control(cancel, budget).check()
}

fn embed_query(
    model: &dyn SearchEmbedder,
    query: &str,
    control: &EmbeddingControl<'_>,
) -> Result<Vec<i8>, EmbedError> {
    control.check()?;
    let mut vectors = model.embed(&[query.to_owned()], InputKind::Query, control)?;
    control.check()?;
    if vectors.len() != 1 {
        return Err(EmbedError::Runtime(
            "query embedding returned the wrong batch size".into(),
        ));
    }
    Ok(vectors.remove(0).values)
}

fn query_result(result: Result<Vec<i8>, EmbedError>) -> Result<Option<Vec<i8>>, EmbedError> {
    match result {
        Ok(vector) => Ok(Some(vector)),
        Err(EmbedError::ModelUnavailable(_) | EmbedError::Runtime(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search_vectors::{Capacity, StoreUnavailable};
    use crate::store::topology::StorageBusy;

    fn service() -> (tempfile::TempDir, SearchService) {
        let directory = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let service =
            SearchService::new(roots, SearchConfig::from_values(None, Some("cpu")).unwrap())
                .unwrap();
        (directory, service)
    }

    struct ConstantFactory;
    struct ConstantEmbedder;
    impl crate::search_embed::SearchEmbedderFactory for ConstantFactory {
        fn load(&self, _: &EmbeddingControl<'_>) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
            Ok(Box::new(ConstantEmbedder))
        }
    }
    impl SearchEmbedder for ConstantEmbedder {
        fn embed(
            &self,
            _: &[String],
            _: InputKind,
            control: &EmbeddingControl<'_>,
        ) -> Result<Vec<crate::search_embed::QuantizedVector>, EmbedError> {
            control.check()?;
            Ok(vec![crate::search_embed::QuantizedVector {
                values: vec![1; DIMENSIONS],
            }])
        }
    }

    #[test]
    fn successful_query_readiness_is_per_call_and_runtime_fault_is_lexical_only() {
        let (_directory, mut service) = service();
        let model = SearchModel::with_embedder_for_tests(Arc::new(ConstantFactory)).unwrap();
        service.status = model.status_handle();
        service.model = Mutex::new(model);
        let cancel = Arc::new(AtomicBool::new(false));
        let query = service
            .query_embedding(
                "query",
                &cancel,
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
            .unwrap();
        assert!(query.model_id().is_some());
        assert_eq!(query.status(2, 1, true).state, SearchIndexState::Building);
        assert_eq!(query.status(2, 1, false).state, SearchIndexState::Partial);
        assert_eq!(query.status(2, 2, false).state, SearchIndexState::Complete);
        assert_eq!(
            service
                .status_handle()
                .snapshot(CardFormat::V1.as_str(), 2, 2, LexicalState::Complete, false)
                .state,
            SearchIndexState::Unavailable
        );
        cancel.store(true, Ordering::Release);
        assert!(matches!(
            service.query_embedding(
                "cancelled",
                &cancel,
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None
            ),
            Err(EmbedError::Cancelled)
        ));
        assert!(
            query_result(Err(EmbedError::Runtime("device fault".into())))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            query_result(Err(EmbedError::InvalidInput("input".into()))),
            Err(EmbedError::InvalidInput(_))
        ));
        assert_eq!(service.admission.lock().unwrap().phase, ModelPhase::Idle);
    }

    struct FaultFactory;
    struct FaultEmbedder;
    impl crate::search_embed::SearchEmbedderFactory for FaultFactory {
        fn load(&self, _: &EmbeddingControl<'_>) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
            Ok(Box::new(FaultEmbedder))
        }
    }
    impl SearchEmbedder for FaultEmbedder {
        fn embed(
            &self,
            _: &[String],
            _: InputKind,
            _: &EmbeddingControl<'_>,
        ) -> Result<Vec<crate::search_embed::QuantizedVector>, EmbedError> {
            Err(EmbedError::Runtime("inference device fault".into()))
        }
    }
    #[test]
    fn runtime_inference_failure_returns_unavailable_query_without_a_storage_failure() {
        let (_directory, mut service) = service();
        let model = SearchModel::with_embedder_for_tests(Arc::new(FaultFactory)).unwrap();
        service.status = model.status_handle();
        service.model = Mutex::new(model);
        let query = service
            .query_embedding(
                "query",
                &Arc::new(AtomicBool::new(false)),
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
            .unwrap();
        assert!(query.vector.is_none());
        assert!(query.model_id().is_none());
        assert_eq!(
            query.status(5, 3, true).state,
            SearchIndexState::Unavailable
        );
        assert_eq!(query.status(5, 3, true).backend, Some(Backend::Cpu));
        assert_eq!(service.admission.lock().unwrap().phase, ModelPhase::Idle);
    }

    #[test]
    fn only_loading_is_lexical_only() {
        let (_directory, service) = service();
        service.admission.lock().unwrap().phase = ModelPhase::Loading;
        let query = service
            .query_embedding(
                "query",
                &Arc::new(AtomicBool::new(false)),
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
            .unwrap();
        assert!(query.vector.is_none());
        assert!(query.model_id().is_none());
        assert_eq!(
            query.status(3, 2, true).state,
            SearchIndexState::Unavailable
        );
        assert_eq!(query.status(3, 2, true).backend, Some(Backend::Cpu));
    }

    #[test]
    fn original_deadline_removes_only_its_queued_request() {
        let (_directory, service) = service();
        service.admission.lock().unwrap().phase = ModelPhase::Ready;
        let (reply, survivor) = mpsc::channel();
        service
            .admission
            .lock()
            .unwrap()
            .queries
            .push_back(QueryRequest {
                id: 99,
                text: "survives".into(),
                cancel: Arc::new(AtomicBool::new(false)),
                deadline: std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                budget: None,
                reply,
            });
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(matches!(
            service.query_embedding("expired", &cancel, std::time::Instant::now(), None),
            Err(EmbedError::Cancelled)
        ));
        assert!(cancel.load(Ordering::Acquire));
        let admission = service.admission.lock().unwrap();
        assert_eq!(admission.queries.len(), 1);
        assert_eq!(admission.queries[0].id, 99);
        drop(admission);
        service.serve_queries(&ConstantEmbedder);
        assert!(survivor.recv().unwrap().unwrap().is_some());
        assert!(service.admission.lock().unwrap().queries.is_empty());
    }

    #[test]
    fn cancelled_queued_query_is_skipped_and_model_owner_continues() {
        let (_directory, service) = service();
        service.admission.lock().unwrap().phase = ModelPhase::Ready;
        let cancel = Arc::new(AtomicBool::new(true));
        let (reply, result) = mpsc::channel();
        service
            .admission
            .lock()
            .unwrap()
            .queries
            .push_back(QueryRequest {
                id: 1,
                text: "cancelled".into(),
                cancel,
                deadline: std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                budget: None,
                reply,
            });
        service.serve_queries(&ConstantEmbedder);
        assert!(matches!(result.recv().unwrap(), Err(EmbedError::Cancelled)));
        assert!(service.admission.lock().unwrap().queries.is_empty());
    }

    fn fake_budget() -> (Arc<Mutex<std::time::Instant>>, QueryBudget) {
        let now = std::time::Instant::now();
        let clock = Arc::new(Mutex::new(now));
        let observed = clock.clone();
        let budget = QueryBudget::new(
            now + std::time::Duration::from_secs(3),
            Arc::new(move || *observed.lock().unwrap()),
        );
        (clock, budget)
    }

    fn advance_budget(service: &SearchService, clock: &Mutex<std::time::Instant>) {
        let _admission = service.admission.lock().unwrap();
        *clock.lock().unwrap() += std::time::Duration::from_secs(4);
        service.model_changed.notify_all();
    }

    fn wait_for_queue(service: &SearchService, count: usize) {
        let deadline = std::time::Instant::now() + crate::testing::HANG_BACKSTOP;
        let mut admission = service.admission.lock().unwrap();
        while admission.queries.len() != count {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            assert!(
                !remaining.is_zero(),
                "query queue did not reach expected size"
            );
            admission = service
                .model_changed
                .wait_timeout(admission, remaining)
                .unwrap()
                .0;
        }
    }

    #[test]
    fn queued_budget_removal_preserves_fill_and_surviving_fifo_query() {
        let (_directory, service) = service();
        let service = Arc::new(service);
        let (clock, budget) = fake_budget();
        let (entered, ready) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        let fill_service = service.clone();
        // A real model owner is held at the same boundary as a document batch.
        let fill_cancel = Arc::new(AtomicBool::new(false));
        let fill_flag = fill_cancel.clone();
        let fill = std::thread::spawn(move || {
            let _job = ModelJob(&fill_service);
            let _owner = fill_service.model.lock().unwrap();
            fill_service.publish_ready();
            entered.send(()).unwrap();
            release.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
            assert!(!fill_flag.load(Ordering::Acquire));
            fill_service.finish_queries(&ConstantEmbedder);
        });
        ready.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let expired_service = service.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let expired_cancel = cancel.clone();
        let expired = std::thread::spawn(move || {
            expired_service.query_embedding(
                "expired",
                &expired_cancel,
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                Some(budget),
            )
        });
        wait_for_queue(&service, 1);
        let surviving_service = service.clone();
        let survivor = std::thread::spawn(move || {
            surviving_service.query_embedding(
                "survives",
                &Arc::new(AtomicBool::new(false)),
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
        });
        wait_for_queue(&service, 2);
        advance_budget(&service, &clock);
        assert!(matches!(
            expired.join().unwrap(),
            Err(EmbedError::BudgetExceeded)
        ));
        assert!(!cancel.load(Ordering::Acquire));
        assert!(!fill_cancel.load(Ordering::Acquire));
        {
            let admission = service.admission.lock().unwrap();
            assert_eq!(admission.queries.len(), 1);
            assert_eq!(admission.queries.front().unwrap().text, "survives");
        }
        resume.send(()).unwrap();
        assert!(survivor.join().unwrap().unwrap().vector.is_some());
        fill.join().unwrap();
        assert_eq!(service.admission.lock().unwrap().phase, ModelPhase::Idle);
    }

    struct GatedFactory {
        entered: mpsc::Sender<String>,
        resume: Arc<Mutex<mpsc::Receiver<()>>>,
        drops: Arc<AtomicU64>,
        loads: Arc<AtomicU64>,
    }
    struct GatedEmbedder {
        entered: mpsc::Sender<String>,
        resume: Arc<Mutex<mpsc::Receiver<()>>>,
        drops: Arc<AtomicU64>,
    }
    impl crate::search_embed::SearchEmbedderFactory for GatedFactory {
        fn load(
            &self,
            control: &EmbeddingControl<'_>,
        ) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
            control.check()?;
            self.loads.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(GatedEmbedder {
                entered: self.entered.clone(),
                resume: self.resume.clone(),
                drops: self.drops.clone(),
            }))
        }
    }
    impl Drop for GatedEmbedder {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::AcqRel);
        }
    }
    impl SearchEmbedder for GatedEmbedder {
        fn embed(
            &self,
            texts: &[String],
            _: InputKind,
            control: &EmbeddingControl<'_>,
        ) -> Result<Vec<crate::search_embed::QuantizedVector>, EmbedError> {
            self.entered.send(texts[0].clone()).unwrap();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(crate::testing::HANG_BACKSTOP)
                .unwrap();
            control.check()?;
            Ok(vec![crate::search_embed::QuantizedVector {
                values: vec![1; DIMENSIONS],
            }])
        }
    }

    type GatedServiceFixture = (
        tempfile::TempDir,
        Arc<SearchService>,
        mpsc::Receiver<String>,
        mpsc::Sender<()>,
        Arc<AtomicU64>,
        Arc<AtomicU64>,
    );

    fn gated_service() -> GatedServiceFixture {
        let (directory, mut service) = service();
        let (entered, started) = mpsc::channel();
        let (resume, release) = mpsc::channel();
        let drops = Arc::new(AtomicU64::new(0));
        let loads = Arc::new(AtomicU64::new(0));
        let model = SearchModel::with_embedder_for_tests(Arc::new(GatedFactory {
            entered,
            resume: Arc::new(Mutex::new(release)),
            drops: drops.clone(),
            loads: loads.clone(),
        }))
        .unwrap();
        service.status = model.status_handle();
        service.model = Mutex::new(model);
        (directory, Arc::new(service), started, resume, drops, loads)
    }

    #[test]
    fn running_budget_waits_for_cooperative_ack_and_retains_verified_weights() {
        let (_directory, service, started, resume, drops, _loads) = gated_service();
        let (clock, budget) = fake_budget();
        let cancel = Arc::new(AtomicBool::new(false));
        let caller_cancel = cancel.clone();
        let caller_service = service.clone();
        let (reply, result) = mpsc::channel();
        let caller = std::thread::spawn(move || {
            let result = caller_service.query_embedding(
                "running",
                &caller_cancel,
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                Some(budget),
            );
            reply.send(result).unwrap();
        });
        assert_eq!(
            started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            "running"
        );
        advance_budget(&service, &clock);
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert_eq!(drops.load(Ordering::Acquire), 0);
        resume.send(()).unwrap();
        assert!(matches!(
            result.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            Err(EmbedError::BudgetExceeded)
        ));
        caller.join().unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert!(!cancel.load(Ordering::Acquire));
        assert_eq!(service.admission.lock().unwrap().phase, ModelPhase::Idle);
    }

    #[test]
    fn query_running_under_fill_acknowledges_budget_without_cancelling_fill() {
        let (_directory, service, started, resume, drops, loads) = gated_service();
        let (fill_ready, ready) = mpsc::channel();
        let (serve, admitted) = mpsc::channel();
        let fill_service = service.clone();
        let fill_cancel = Arc::new(AtomicBool::new(false));
        let fill_flag = fill_cancel.clone();
        let fill = std::thread::spawn(move || {
            fill_service.with_model_job(&fill_flag, |model| {
                fill_ready.send(()).unwrap();
                admitted
                    .recv_timeout(crate::testing::HANG_BACKSTOP)
                    .unwrap();
                fill_service.serve_queries(model);
                assert!(!fill_flag.load(Ordering::Acquire));
                Ok(())
            })
        });
        ready.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let (clock, budget) = fake_budget();
        let cancel = Arc::new(AtomicBool::new(false));
        let caller_cancel = cancel.clone();
        let query_service = service.clone();
        let (reply, result) = mpsc::channel();
        let query = std::thread::spawn(move || {
            reply
                .send(query_service.query_embedding(
                    "fill query",
                    &caller_cancel,
                    std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                    Some(budget),
                ))
                .unwrap();
        });
        wait_for_queue(&service, 1);
        serve.send(()).unwrap();
        assert_eq!(
            started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            "fill query"
        );
        advance_budget(&service, &clock);
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(service.admission.lock().unwrap().queries.is_empty());
        assert_eq!(drops.load(Ordering::Acquire), 0);
        resume.send(()).unwrap();
        assert!(matches!(
            result.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            Err(EmbedError::BudgetExceeded)
        ));
        query.join().unwrap();
        fill.join().unwrap().unwrap();
        assert!(!cancel.load(Ordering::Acquire));
        assert!(!fill_cancel.load(Ordering::Acquire));
        assert_eq!(loads.load(Ordering::Acquire), 1);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert_eq!(service.admission.lock().unwrap().phase, ModelPhase::Idle);
    }

    #[test]
    fn unavailable_budget_status_keeps_reason_and_exact_native_count() {
        let (_clock, budget) = fake_budget();
        let query = SearchQuery::budget_exceeded(Some(Backend::Cpu), budget);
        let status = query.status(7, 3, true);
        assert!(query.vector.is_none());
        assert!(query.model_id().is_none());
        assert!(query.budget().is_some());
        assert_eq!(status.state, SearchIndexState::Unavailable);
        assert_eq!(status.declarations, 7);
        assert_eq!(status.embedded, 0);
        assert_eq!(
            status.unavailable_reason,
            Some(SearchUnavailableReason::VectorBudgetExceeded)
        );
    }

    #[test]
    fn direct_owner_returns_before_sibling_and_fifo_ticket_reuses_model() {
        let (_directory, service, started, resume, drops, loads) = gated_service();
        let owner_service = service.clone();
        let first = std::thread::spawn(move || {
            owner_service.query_embedding(
                "first",
                &Arc::new(AtomicBool::new(false)),
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
        });
        assert_eq!(
            started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            "first"
        );
        let sibling_service = service.clone();
        let second = std::thread::spawn(move || {
            sibling_service.query_embedding(
                "second",
                &Arc::new(AtomicBool::new(false)),
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
        });
        wait_for_queue(&service, 1);
        resume.send(()).unwrap();
        assert_eq!(
            started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            "second"
        );
        assert!(first.join().unwrap().unwrap().vector.is_some());
        assert_eq!(loads.load(Ordering::Acquire), 1);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        resume.send(()).unwrap();
        assert!(second.join().unwrap().unwrap().vector.is_some());
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert!(service.admission.lock().unwrap().queries.is_empty());
        assert_eq!(service.admission.lock().unwrap().phase, ModelPhase::Idle);
    }

    #[test]
    fn existing_fill_driver_evicts_resident_after_ten_idle_minutes_and_use_resets_timer() {
        let (_directory, mut service, started, resume, drops, loads) = gated_service();
        let now = std::time::Instant::now();
        let clock = Arc::new(Mutex::new(now));
        let observed = clock.clone();
        Arc::get_mut(&mut service)
            .unwrap()
            .set_model_clock_for_tests(Arc::new(move || *observed.lock().unwrap()));
        let fill = super::super::search_fill::SearchFill::start(service.clone()).unwrap();
        let query = |text: &str| {
            resume.send(()).unwrap();
            service
                .query_embedding(
                    text,
                    &Arc::new(AtomicBool::new(false)),
                    std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                    None,
                )
                .unwrap()
        };
        assert!(query("cold").vector.is_some());
        assert_eq!(
            started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            "cold"
        );
        assert_eq!(loads.load(Ordering::Acquire), 1);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        *clock.lock().unwrap() = now + std::time::Duration::from_secs(599);
        assert!(!service.begin_idle_expiry());
        assert!(query("warm").vector.is_some());
        assert_eq!(
            started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            "warm"
        );
        assert_eq!(loads.load(Ordering::Acquire), 1);
        *clock.lock().unwrap() = now + std::time::Duration::from_secs(600);
        assert!(
            !service.begin_idle_expiry(),
            "warm use reset the old expiry"
        );
        assert_eq!(
            service.idle_remaining(),
            Some(std::time::Duration::from_secs(599))
        );
        *clock.lock().unwrap() = now + std::time::Duration::from_secs(1199);
        service.wake_model_clock_for_tests();
        service.wait_for_model_eviction_for_tests();
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert!(!service.model.lock().unwrap().has_resident());
        assert!(query("after eviction").vector.is_some());
        assert_eq!(
            started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap(),
            "after eviction"
        );
        assert_eq!(loads.load(Ordering::Acquire), 2);
        drop(fill);
    }

    #[test]
    fn matrix_scoring_use_defers_expiry_and_resets_same_idle_cutoff() {
        let (_directory, mut service, started, resume, _drops, _loads) = gated_service();
        let now = std::time::Instant::now();
        let clock = Arc::new(Mutex::new(now));
        let observed = clock.clone();
        Arc::get_mut(&mut service)
            .unwrap()
            .set_model_clock_for_tests(Arc::new(move || *observed.lock().unwrap()));
        resume.send(()).unwrap();
        service
            .query_embedding(
                "resident",
                &Arc::new(AtomicBool::new(false)),
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
            .unwrap();
        started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let use_guard = service.begin_head_use().unwrap();
        *clock.lock().unwrap() = now + MODEL_IDLE;
        assert!(!service.begin_idle_expiry());
        drop(use_guard);
        assert_eq!(service.idle_remaining(), Some(MODEL_IDLE));
        *clock.lock().unwrap() += MODEL_IDLE;
        assert!(service.begin_idle_expiry());
        service.finish_idle_expiry();
    }

    #[test]
    fn queued_ticket_prevents_idle_claim_until_its_own_cleanup() {
        let (_directory, mut service, started, resume, drops, _loads) = gated_service();
        let now = std::time::Instant::now();
        let clock = Arc::new(Mutex::new(now));
        let observed = clock.clone();
        Arc::get_mut(&mut service)
            .unwrap()
            .set_model_clock_for_tests(Arc::new(move || *observed.lock().unwrap()));
        resume.send(()).unwrap();
        service
            .query_embedding(
                "resident",
                &Arc::new(AtomicBool::new(false)),
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
            .unwrap();
        started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        *clock.lock().unwrap() += MODEL_IDLE;
        let (reply, _receiver) = mpsc::channel();
        service
            .admission
            .lock()
            .unwrap()
            .queries
            .push_back(QueryRequest {
                id: 101,
                text: "queued".into(),
                cancel: Arc::new(AtomicBool::new(false)),
                deadline: std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                budget: None,
                reply,
            });
        assert!(!service.begin_idle_expiry());
        assert_eq!(drops.load(Ordering::Acquire), 0);
        drop(QueuedQuery {
            service: &service,
            id: 101,
        });
        assert!(service.begin_idle_expiry());
        service.finish_idle_expiry();
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }

    #[test]
    fn active_fill_defers_idle_eviction_until_model_use_finishes() {
        let (_directory, mut service, started, resume, drops, loads) = gated_service();
        let now = std::time::Instant::now();
        let clock = Arc::new(Mutex::new(now));
        let observed = clock.clone();
        Arc::get_mut(&mut service)
            .unwrap()
            .set_model_clock_for_tests(Arc::new(move || *observed.lock().unwrap()));
        resume.send(()).unwrap();
        service
            .query_embedding(
                "cold",
                &Arc::new(AtomicBool::new(false)),
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP,
                None,
            )
            .unwrap();
        started.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let (entered, ready) = mpsc::channel();
        let (finish, release) = mpsc::channel();
        let fill_service = service.clone();
        let fill = std::thread::spawn(move || {
            fill_service.with_model_job(&AtomicBool::new(false), |_| {
                entered.send(()).unwrap();
                release.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
                Ok(())
            })
        });
        ready.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        *clock.lock().unwrap() = now + MODEL_IDLE;
        assert!(!service.begin_idle_expiry());
        assert_eq!(service.idle_remaining(), None);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        finish.send(()).unwrap();
        fill.join().unwrap().unwrap();
        assert_eq!(loads.load(Ordering::Acquire), 1);
        assert_eq!(service.idle_remaining(), Some(MODEL_IDLE));
        *clock.lock().unwrap() += MODEL_IDLE;
        assert!(service.begin_idle_expiry());
        service.finish_idle_expiry();
        assert_eq!(drops.load(Ordering::Acquire), 1);
    }

    #[test]
    fn query_cache_decode_misses_and_database_faults_are_distinct() {
        let (_directory, service) = service();
        let model = "a".repeat(64);
        let card = "b".repeat(64);
        service
            .with_vector_writer(|writer| writer.put(&model, &card, &[1, 2]))
            .unwrap();
        let epoch = service.cache_epoch();
        for error in [
            anyhow::Error::new(StorageBusy),
            anyhow::Error::new(Capacity),
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::Other))
                .context(StoreUnavailable),
        ] {
            service
                .with_vector_snapshot::<Vec<bool>>(|_| Err(error))
                .unwrap_err();
            assert!(service.vectors.lock().unwrap().is_some());
            assert_eq!(service.cache_epoch(), epoch);
        }
        let path = service.roots.vectors_db();
        {
            let path = std::fs::canonicalize(path).unwrap();
            let _witness = crate::store::retained_sqlite_file(&path, true, false, true).unwrap();
            let db = crate::store::protected_sqlite_open(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )
            .unwrap();
            db.execute("UPDATE vectors SET vector=zeroblob(2)", [])
                .unwrap();
            service
                .with_vector_snapshot(|snapshot| {
                    let snapshot = snapshot.unwrap();
                    assert_eq!(snapshot.entry_count(), 1);
                    assert!(snapshot.get(&model, &card)?.is_none());
                    Ok(())
                })
                .unwrap();
            db.pragma_update(None, "user_version", 2).unwrap();
        }
        let error = service
            .with_vector_snapshot::<()>(|_| panic!("invalid database is not absence"))
            .unwrap_err();
        assert!(error.is::<StoreUnavailable>());
        assert!(is_corrupt_cache(&error));
        assert!(service.vectors.lock().unwrap().is_none());
        assert_eq!(service.cache_epoch(), epoch + 1);
        service
            .with_vector_writer(|writer| {
                assert_eq!(writer.snapshot()?.entry_count(), 0);
                writer.put(&model, &card, &[2; DIMENSIONS])
            })
            .unwrap();
        let vector = service
            .with_vector_snapshot(|snapshot| snapshot.unwrap().get(&model, &card))
            .unwrap();
        assert_eq!(vector, Some(vec![2; DIMENSIONS]));
    }

    #[test]
    fn wrong_dimension_vectors_are_fill_misses_until_replaced() {
        let (_directory, service) = service();
        let model = "a".repeat(64);
        let card = "b".repeat(64);
        service
            .with_vector_writer(|writer| writer.put(&model, &card, &[1; DIMENSIONS - 1]))
            .unwrap();
        assert_eq!(service.missing_vectors(&model, &[&card]).unwrap(), [true]);
        service
            .with_vector_writer(|writer| writer.put(&model, &card, &[1; DIMENSIONS]))
            .unwrap();
        assert_eq!(service.missing_vectors(&model, &[&card]).unwrap(), [false]);
    }

    #[test]
    fn corrupt_miss_read_drops_cached_writer_before_next_job_repairs() {
        let directory = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let service = SearchService::new(
            roots.clone(),
            SearchConfig::from_values(None, Some("cpu")).unwrap(),
        )
        .unwrap();
        let model = "a".repeat(64);
        let card = "b".repeat(64);
        service
            .with_vector_writer(|writer| writer.put(&model, &card, &[1]))
            .unwrap();
        {
            // Change the disposable format through a protected SQLite handle,
            // never by opening/closing a raw descriptor under the live writer.
            let path = std::fs::canonicalize(roots.vectors_db()).unwrap();
            let _witness = crate::store::retained_sqlite_file(&path, true, false, true).unwrap();
            let db = crate::store::protected_sqlite_open(
                &path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
            )
            .unwrap();
            db.pragma_update(None, "user_version", 2).unwrap();
        }
        let error = service.missing_vectors(&model, &[&card]).unwrap_err();
        assert!(error.is::<StoreUnavailable>());
        assert!(is_corrupt_cache(&error));
        assert!(service.vectors.lock().unwrap().is_none());
        service
            .with_vector_writer(|writer| {
                assert_eq!(writer.snapshot()?.entry_count(), 0);
                writer.put(&model, &card, &[2; DIMENSIONS])
            })
            .unwrap();
        assert_eq!(service.missing_vectors(&model, &[&card]).unwrap(), [false]);
    }

    #[test]
    fn corrupt_job_drops_cached_writer_and_next_job_recreates() {
        let directory = tempfile::tempdir().unwrap();
        let roots = TopologyRoots::isolated_for_tests(
            directory.path().join("cache"),
            directory.path().join("data"),
        );
        let service = SearchService::new(
            roots.clone(),
            SearchConfig::from_values(None, Some("cpu")).unwrap(),
        )
        .unwrap();
        let model = "a".repeat(64);
        let card = "b".repeat(64);
        for code in [rusqlite::ffi::SQLITE_CORRUPT, rusqlite::ffi::SQLITE_NOTADB] {
            service
                .with_vector_writer(|writer| writer.put(&model, &card, &[1]))
                .unwrap();
            // Inject the same typed failure a cache operation returns. No raw
            // file handle is opened or closed beneath a live SQLite writer.
            let error = service
                .with_vector_writer::<()>(|_| {
                    Err(anyhow::Error::new(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error::new(code),
                        None,
                    ))
                    .context(StoreUnavailable))
                })
                .unwrap_err();
            assert!(error.is::<StoreUnavailable>());
            assert!(service.vectors.lock().unwrap().is_none());
            // With the writer confirmed dropped, a broken disposable file is
            // safe to install. The next job must reopen and repair it.
            std::fs::write(roots.vectors_db(), b"not a SQLite database").unwrap();
            service
                .with_vector_writer(|writer| {
                    assert_eq!(writer.snapshot()?.entry_count(), 0);
                    writer.put(&model, &card, &[2, -3])?;
                    assert_eq!(writer.snapshot()?.get(&model, &card)?, Some(vec![2, -3]));
                    Ok(())
                })
                .unwrap();
        }

        for error in [
            anyhow::Error::new(StorageBusy),
            anyhow::Error::new(Capacity),
            anyhow::Error::new(std::io::Error::from(std::io::ErrorKind::Other))
                .context(StoreUnavailable),
        ] {
            service
                .with_vector_writer::<()>(|_| Err(error))
                .unwrap_err();
            assert!(service.vectors.lock().unwrap().is_some());
        }
    }
}
