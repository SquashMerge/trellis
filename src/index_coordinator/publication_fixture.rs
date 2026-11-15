//! The offline native publication fixtures that measure before they publish. Each one runs
//! the indexing side of a publication (the full document spool, or the projection of a local
//! edit's changed documents) and hands the result to storage's ordinary publisher, as
//! [`super::IndexJobCoordinator`] does, so they live with it. The fixtures that inject a
//! fault into the publisher's own transaction stay in storage.
use crate::{
    capture::Capture,
    indexer,
    model::{CancelFlag, IndexOptions, IndexPin},
    native_evidence,
    store::{Store, topology::WriterGuard},
};
use anyhow::{Context, Result};
use std::path::Path;

impl Store {
    #[doc(hidden)]
    pub fn publish_stream_native_only_for_tests(
        &self,
        options: &IndexOptions,
        capture: &Capture,
        leader: &WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let spool = native_evidence::spool_full_capture(
            options,
            capture,
            Path::new(self.workspace_root()),
            self.root_id(),
            cancel,
            &|_| {},
            |_, _| {},
            &self.native_spool_dir()?,
        )?;
        self.publish_validated_stream_recovery(
            &spool,
            capture,
            leader,
            self.publication_index_baseline()?,
            cancel,
        )
    }

    /// A document-local native publication, which rejoins selected batches through the
    /// witness path where it applies.
    #[doc(hidden)]
    pub fn publish_local_for_tests(
        &self,
        options: &IndexOptions,
        capture: &Capture,
        leader: &WriterGuard,
        cancel: &CancelFlag,
    ) -> Result<IndexPin> {
        let expected = self.publication_index_baseline()?;
        let prepared = self
            .prepare_local_revision(capture, &expected, cancel)?
            .context("document-local publication unavailable")?;
        let changed = indexer::project_native_documents(
            options,
            capture,
            &prepared.native,
            &prepared.changed_paths,
            cancel,
            &|_| {},
        )?;
        let graph = self.compose_local_revision(capture, &prepared, changed, cancel)?;
        self.publish_local_native_recovery(&graph, capture, prepared, leader, expected, cancel)
    }
}
