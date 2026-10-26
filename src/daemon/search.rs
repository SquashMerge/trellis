//! Daemon-wide local model and vector-cache ownership, independent of checkouts.
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc,
    },
};

use crate::{
    search_card::CardFormat,
    search_embed::{Backend, DIMENSIONS, EmbedError, InputKind, SearchEmbedder},
    search_model::{
        LexicalState, SearchConfig, SearchIndexState, SearchIndexStatus, SearchModel,
        SearchModelStatus,
    },
    search_vectors::{
        VectorCacheReader, VectorCacheReport, VectorCacheWriter, VectorSnapshot, is_corrupt_cache,
        report_existing,
    },
    store::topology::TopologyRoots,
};
use anyhow::Result;

/// One call's model decision and query vector. Readiness is captured when the
/// query owns the model job, not sampled from a later fill/query job.
pub struct SearchQuery {
    pub vector: Option<Vec<i8>>,
    status: SearchIndexStatus,
}

impl SearchQuery {
    /// Standalone/test registries may have no daemon search service or selected
    /// backend. A configured service instead preserves its selected backend.
    pub fn unavailable(backend: Option<Backend>) -> Self {
        Self {
            vector: None,
            status: SearchIndexStatus {
                state: SearchIndexState::Unavailable,
                model_id: None,
                model_name: None,
                backend,
                declarations: 0,
                embedded: 0,
                lexical: LexicalState::Complete,
            },
        }
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
    reply: mpsc::Sender<Result<Option<Vec<i8>>, EmbedError>>,
}

#[derive(Default)]
struct Admission {
    next_id: u64,
    phase: ModelPhase,
    queries: VecDeque<QueryRequest>,
}

struct QueuedQuery<'a> {
    service: &'a SearchService,
    id: u64,
}
impl Drop for QueuedQuery<'_> {
    fn drop(&mut self) {
        self.service
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .queries
            .retain(|request| request.id != self.id);
    }
}

/// Releases admission and every queued caller on all exits, including panic.
struct ModelJob<'a>(&'a SearchService);
impl Drop for ModelJob<'_> {
    fn drop(&mut self) {
        let mut admission = self
            .0
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        admission.phase = ModelPhase::Idle;
        for request in admission.queries.drain(..) {
            let _ = request
                .reply
                .send(if request.cancel.load(Ordering::Acquire) {
                    Err(EmbedError::Cancelled)
                } else {
                    Ok(None)
                });
        }
        self.0.model_changed.notify_all();
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

    pub(crate) fn model_configured(&self) -> bool {
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
    ) -> Result<SearchQuery, EmbedError> {
        if cancel.load(Ordering::Acquire) {
            return Err(EmbedError::Cancelled);
        }
        let receiver = {
            let mut admission = self
                .admission
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            while admission.phase == ModelPhase::Closing {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() || cancel.load(Ordering::Acquire) {
                    return Err(EmbedError::Cancelled);
                }
                admission = self
                    .model_changed
                    .wait_timeout(admission, remaining)
                    .unwrap_or_else(|error| error.into_inner())
                    .0;
            }
            match admission.phase {
                ModelPhase::Closing => unreachable!("closing owner was awaited"),
                ModelPhase::Loading => {
                    return Ok(SearchQuery::unavailable(Some(self.status.backend())));
                }
                ModelPhase::Ready => {
                    let (reply, receiver) = mpsc::channel();
                    let id = admission.next_id;
                    admission.next_id += 1;
                    admission.queries.push_back(QueryRequest {
                        id,
                        text: query.to_owned(),
                        cancel: cancel.clone(),
                        reply,
                    });
                    self.model_changed.notify_all();
                    Some((id, receiver))
                }
                ModelPhase::Idle => {
                    admission.phase = ModelPhase::Loading;
                    None
                }
            }
        };
        let vector = if let Some((id, receiver)) = receiver {
            let _request = QueuedQuery { service: self, id };
            match receiver
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            {
                Ok(result) => result?,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    cancel.store(true, Ordering::Release);
                    return Err(EmbedError::Cancelled);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => None,
            }
        } else {
            let _job = ModelJob(self);
            let mut owner = self.model.lock().unwrap_or_else(|error| error.into_inner());
            query_result(owner.with_job(cancel, |model| {
                self.publish_ready();
                let result = embed_query(model, query, cancel);
                self.finish_queries(model);
                result
            }))?
        };
        if cancel.load(Ordering::Acquire) {
            return Err(EmbedError::Cancelled);
        }
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
        })
    }

    fn publish_ready(&self) {
        self.admission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase = ModelPhase::Ready;
        self.model_changed.notify_all();
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
            let result = if request.cancel.load(Ordering::Acquire) {
                Err(EmbedError::Cancelled)
            } else {
                query_result(embed_query(model, &request.text, &request.cancel))
            };
            let _ = request.reply.send(result);
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
                // Close admission before the loaded model drops. Idle is only
                // published by ModelJob after the model owner releases its lock.
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
        let result = (|| {
            let Some(mut reader) = VectorCacheReader::open_existing(&self.roots.vectors_db())?
            else {
                return operation(None);
            };
            let snapshot = reader.snapshot()?;
            operation(Some(&snapshot))
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
            while admission.phase != ModelPhase::Idle {
                admission = self
                    .model_changed
                    .wait(admission)
                    .unwrap_or_else(|error| error.into_inner());
            }
            admission.phase = ModelPhase::Loading;
        }
        let _job = ModelJob(self);
        let mut owner = self.model.lock().unwrap_or_else(|error| error.into_inner());
        owner.with_job(cancel, |model| {
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

fn embed_query(
    model: &dyn SearchEmbedder,
    query: &str,
    cancel: &AtomicBool,
) -> Result<Vec<i8>, EmbedError> {
    let mut vectors = model.embed(&[query.to_owned()], InputKind::Query, cancel)?;
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
        fn load(&self, _: &AtomicBool) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
            Ok(Box::new(ConstantEmbedder))
        }
    }
    impl SearchEmbedder for ConstantEmbedder {
        fn embed(
            &self,
            _: &[String],
            _: InputKind,
            cancel: &AtomicBool,
        ) -> Result<Vec<crate::search_embed::QuantizedVector>, EmbedError> {
            if cancel.load(Ordering::Acquire) {
                return Err(EmbedError::Cancelled);
            }
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
                std::time::Instant::now() + crate::testing::HANG_BACKSTOP
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
        fn load(&self, _: &AtomicBool) -> Result<Box<dyn SearchEmbedder>, EmbedError> {
            Ok(Box::new(FaultEmbedder))
        }
    }
    impl SearchEmbedder for FaultEmbedder {
        fn embed(
            &self,
            _: &[String],
            _: InputKind,
            _: &AtomicBool,
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
                reply,
            });
        let cancel = Arc::new(AtomicBool::new(false));
        assert!(matches!(
            service.query_embedding("expired", &cancel, std::time::Instant::now()),
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
                reply,
            });
        service.serve_queries(&ConstantEmbedder);
        assert!(matches!(result.recv().unwrap(), Err(EmbedError::Cancelled)));
        assert!(service.admission.lock().unwrap().queries.is_empty());
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
