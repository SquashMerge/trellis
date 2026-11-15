//! The offline semantic catch-up fixtures. They drive the checkout worker's own bounded unit
//! ([`super::cooperative_semantic_unit`]), so they live with it; storage keeps only the check
//! of the committed rows.
use super::cooperative_semantic_unit;
use crate::{
    model::{CancelFlag, IndexPin},
    store::{SemanticUnitOutcome, Store, topology::WriterSession},
};
use anyhow::{Result, ensure};
use std::sync::Arc;

impl Store {
    /// Complete an offline fixture's exact target through the ordinary bounded owner units.
    #[doc(hidden)]
    pub fn complete_pending_semantic_for_tests(
        &self,
        expected: IndexPin,
        session: &Arc<WriterSession>,
        cancel: &CancelFlag,
    ) -> Result<Option<IndexPin>> {
        self.verify_writer_session(session)?;
        ensure!(
            self.index_baseline()? == expected,
            "semantic fixture head changed"
        );
        let Some(target) = self.pending_semantic_target()? else {
            return Ok(None);
        };
        ensure!(target.pin == expected, "semantic fixture target changed");
        let mut continuation = None;
        // A unit bound detects a broken fixture; only Published proves completion.
        for _ in 0..10_000 {
            ensure!(
                self.pending_semantic_target()?.as_ref() == Some(&target),
                "semantic fixture target superseded"
            );
            match cooperative_semantic_unit(self, session, &mut continuation, cancel, || true)? {
                SemanticUnitOutcome::Progress => {}
                SemanticUnitOutcome::Published => {
                    let published = self.index_baseline()?;
                    ensure!(
                        published.index_generation == expected.index_generation
                            && published.index_revision == expected.index_revision + 1,
                        "semantic fixture published a different paired pin"
                    );
                    self.ensure_semantic_fixture_committed_for_tests(published, expected, &target)?;
                    return Ok(Some(published));
                }
                other => anyhow::bail!("semantic fixture did not progress: {other:?}"),
            }
        }
        anyhow::bail!("semantic fixture exceeded its unit bound")
    }

    /// Offline fixtures that published native N with their own writer and dropped it: take
    /// the writer again and drive any outstanding exact target at head to its S commit.
    #[doc(hidden)]
    pub fn catch_up_semantic_for_tests(&self, cancel: &CancelFlag) -> Result<IndexPin> {
        let session = Arc::new(WriterSession::writer(self.writer()?));
        let head = self.index_baseline()?;
        Ok(self
            .complete_pending_semantic_for_tests(head, &session, cancel)?
            .unwrap_or(head))
    }
}
