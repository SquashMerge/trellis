//! Daemon-wide local model and vector-cache ownership, independent of checkouts.
use std::{
    path::PathBuf,
    sync::{Mutex, atomic::AtomicBool},
};

use crate::{
    search_embed::{EmbedError, Embedder},
    search_model::{SearchConfig, SearchModel, SearchModelStatus},
    search_vectors::{VectorCacheReport, VectorCacheWriter, report_existing},
    store::topology::TopologyRoots,
};
use anyhow::Result;

pub struct SearchService {
    model: Mutex<SearchModel>,
    status: SearchModelStatus,
    vectors: Mutex<Option<VectorCacheWriter>>,
    roots: TopologyRoots,
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
        let model = SearchModel::new(config)?;
        let status = model.status_handle();
        Ok(Self {
            model: Mutex::new(model),
            status,
            vectors: Mutex::new(None),
            roots,
        })
    }

    pub fn status_handle(&self) -> SearchModelStatus {
        self.status.clone()
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
        operation(writer.as_mut().expect("vector writer initialized"))
    }

    pub fn gc_report(&self) -> Result<crate::store::topology::GcReport> {
        self.roots.gc_report()
    }

    /// Reporting does not acquire the writer lock or create an optional cache.
    pub fn vector_report(&self) -> Result<VectorCacheReport> {
        report_existing(&self.roots.vectors_db())
    }
}
