//! Daemon-wide local model and vector-cache ownership, independent of checkouts.
use std::{
    path::PathBuf,
    sync::{
        Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
};

use crate::{
    search_card::CardFormat,
    search_embed::{Backend, DIMENSIONS, EmbedError, Embedder, InputKind},
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

pub struct SearchService {
    model: Mutex<SearchModel>,
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
        let model = SearchModel::new(config)?;
        let status = model.status_handle();
        Ok(Self {
            model: Mutex::new(model),
            status,
            vectors: Mutex::new(None),
            roots,
            model_configured,
        })
    }

    pub(crate) fn model_configured(&self) -> bool {
        self.model_configured
    }

    pub fn status_handle(&self) -> SearchModelStatus {
        self.status.clone()
    }

    /// Call before acquiring a checkout index snapshot. A query may own a lazy
    /// load/embed/drop job, but never waits for the model owner: a fill or another
    /// query already owning it means lexical-only for this call. In particular,
    /// a failed try_lock proves only that this query cannot run inference.
    pub fn query_embedding(
        &self,
        query: &str,
        cancel: &AtomicBool,
    ) -> Result<SearchQuery, EmbedError> {
        self.query_with(cancel, |owner| {
            owner.with_job(cancel, |model| {
                let mut vectors = model.embed(&[query.to_owned()], InputKind::Query, cancel)?;
                Ok(vectors.remove(0).values)
            })
        })
    }

    fn query_with(
        &self,
        cancel: &AtomicBool,
        operation: impl FnOnce(&mut SearchModel) -> Result<Vec<i8>, EmbedError>,
    ) -> Result<SearchQuery, EmbedError> {
        if cancel.load(Ordering::Acquire) {
            return Err(EmbedError::Cancelled);
        }
        let vector = match self.model.try_lock() {
            Ok(mut owner) => query_result(operation(&mut owner))?,
            Err(TryLockError::Poisoned(error)) => query_result(operation(&mut error.into_inner()))?,
            Err(TryLockError::WouldBlock) => None,
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

    /// All per-call counts and ranking vectors must use this one read snapshot.
    /// Missing cache is ordinary absence; malformed vectors are misses through
    /// VectorSnapshot::get. Actual storage faults and contention remain typed.
    /// This path never takes the writer/model lock or creates the optional cache.
    pub fn with_vector_snapshot<T>(
        &self,
        operation: impl FnOnce(Option<&VectorSnapshot<'_>>) -> Result<T>,
    ) -> Result<T> {
        let Some(mut reader) = VectorCacheReader::open_existing(&self.roots.vectors_db())? else {
            return operation(None);
        };
        let snapshot = reader.snapshot()?;
        operation(Some(&snapshot))
    }

    pub fn with_model_job<T>(
        &self,
        cancel: &AtomicBool,
        operation: impl FnOnce(&Embedder) -> Result<T, EmbedError>,
    ) -> Result<T, EmbedError> {
        self.model
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .with_job(cancel, operation)
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
        }
        let result = operation(writer.as_mut().expect("vector writer initialized"));
        if result.as_ref().is_err_and(is_corrupt_cache) {
            // Reopen/recreate on the next job, not within a failed operation.
            *writer = None;
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
        if result.as_ref().is_err_and(is_corrupt_cache) {
            *self
                .vectors
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = None;
        }
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

fn query_result(result: Result<Vec<i8>, EmbedError>) -> Result<Option<Vec<i8>>, EmbedError> {
    match result {
        Ok(vector) => Ok(Some(vector)),
        Err(EmbedError::ModelUnavailable(_)) => Ok(None),
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

    #[test]
    fn query_admission_never_waits_for_fill_or_borrows_its_readiness() {
        let (_directory, service) = service();
        let _fill_owner = service.model.lock().unwrap();
        let query = service
            .query_with(&AtomicBool::new(false), |_| panic!("owner is occupied"))
            .unwrap();
        assert!(query.vector.is_none());
        assert!(query.model_id().is_none());
        let status = query.status(3, 2, true);
        assert_eq!(
            status.state,
            crate::search_model::SearchIndexState::Unavailable
        );
        assert_eq!(status.embedded, 0);
        assert_eq!(status.backend, Some(crate::search_embed::Backend::Cpu));
    }

    #[test]
    fn query_readiness_is_per_call_and_cancellation_overrides_results() {
        let (_directory, service) = service();
        let cancel = AtomicBool::new(false);
        let query = service.query_with(&cancel, |_| Ok(vec![1, -2])).unwrap();
        assert!(query.model_id().is_some());
        assert_eq!(query.vector.as_deref(), Some([1, -2].as_slice()));
        assert_eq!(
            query.status(2, 1, false).state,
            crate::search_model::SearchIndexState::Partial
        );
        assert_eq!(
            query.status(2, 1, true).state,
            crate::search_model::SearchIndexState::Building
        );
        assert_eq!(
            query.status(2, 2, false).state,
            crate::search_model::SearchIndexState::Complete
        );
        assert_eq!(
            service
                .status_handle()
                .snapshot(CardFormat::V1.as_str(), 2, 2, LexicalState::Complete, false)
                .state,
            crate::search_model::SearchIndexState::Unavailable
        );
        let unavailable = service.query_embedding("query", &cancel).unwrap();
        assert!(unavailable.vector.is_none());
        assert!(
            query.model_id().is_some(),
            "a later unavailable job cannot change an earlier call"
        );
        let result = service.query_with(&cancel, |_| {
            cancel.store(true, Ordering::Release);
            Ok(vec![1])
        });
        assert!(matches!(result, Err(EmbedError::Cancelled)));
        assert!(matches!(
            service.query_with(&cancel, |_| panic!("pre-cancelled")),
            Err(EmbedError::Cancelled)
        ));
        cancel.store(false, Ordering::Release);
        assert!(matches!(
            service.query_with(&cancel, |_| Err(EmbedError::Runtime("inference".into()))),
            Err(EmbedError::Runtime(_))
        ));
        let standalone = SearchQuery::unavailable(None).status(2, 2, true);
        assert_eq!(standalone.backend, None);
        assert_eq!(standalone.embedded, 0);
        assert_eq!(standalone.state, SearchIndexState::Unavailable);
    }

    #[test]
    fn query_cache_decode_misses_and_database_faults_are_distinct() {
        let (_directory, service) = service();
        let model = "a".repeat(64);
        let card = "b".repeat(64);
        service
            .with_vector_writer(|writer| writer.put(&model, &card, &[1, 2]))
            .unwrap();
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
