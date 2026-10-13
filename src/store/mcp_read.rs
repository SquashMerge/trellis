//! Narrow MCP projections over the already admitted selected SQLite snapshot.
use super::*;

impl EvidenceResponse {
    pub fn mcp_language_counts(&self) -> Result<[[u64; 5]; 4]> {
        #[cfg(feature = "test-support")]
        if self
            .store
            .mcp_read_busy_attempts
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count > 0).then(|| count - 1)
            })
            .is_ok()
        {
            return Err(SqliteContention(()).into());
        }
        let selected = self.store.read_revision_for(&self.db, None, true)?;
        let mut counts = [[0_u64; 5]; 4];
        let mut rows = self.db.prepare(
            "SELECT language,coverage_selected,coverage_state FROM revision_documents WHERE revision_id=?1"
        )?;
        let records = rows.query_map([&selected.key], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for record in records {
            let (language, chosen, state) = record?;
            let bucket = match language.as_str() {
                "java" => 0,
                "rust" => 1,
                "python" => 2,
                "javascript" => 3,
                _ => anyhow::bail!("store_unavailable: invalid selected language"),
            };
            let category = if chosen {
                match state.as_str() {
                    "complete" => 1,
                    "partial" => 2,
                    "failed" => 3,
                    _ => anyhow::bail!("store_unavailable: invalid selected coverage"),
                }
            } else {
                4
            };
            counts[bucket][category] += 1;
            if chosen {
                counts[bucket][0] += 1;
            }
        }
        Ok(counts)
    }
}

#[cfg(feature = "test-support")]
impl Store {
    /// Inject direct typed SQLite contention after a W1 selected admission.
    pub fn mcp_busy_reads_for_tests(&self, count: usize) {
        self.mcp_read_busy_attempts.store(count, Ordering::Release);
    }

    /// Use the normal virgin v1 queue staging path before fixture publication.
    pub fn mcp_prepare_virgin_queue_for_tests(&self) -> Result<()> {
        self.prepare_virgin_queue_before_activation()
    }

    /// Fixture-only revision row setup for W1 projection tests. Uses Store's own
    /// cached SQLite connection; never closes another FD on a live index inode.
    pub fn mcp_set_coverage_for_tests(
        &self,
        writer: &topology::WriterGuard,
        path: &str,
        selected: bool,
        state: &str,
    ) -> Result<()> {
        anyhow::ensure!(
            matches!(state, "complete" | "partial" | "failed" | "unsupported"),
            "invalid fixture state"
        );
        writer.belongs_to(&self.identity)?;
        self.identity.verify()?;
        writer.verify()?;
        let mut db = self.cache_write()?;
        let tx = storage_result(db.transaction_with_behavior(TransactionBehavior::Immediate))?;
        let count = storage_result(tx.execute(
            "UPDATE revision_documents SET coverage_selected=?1,coverage_state=?2,coverage_diagnostic=?3 WHERE revision_id=(SELECT 'pin:v1:'||index_generation||':'||index_revision FROM index_metadata WHERE singleton=1) AND path=?4",
            params![selected, state, (state != "complete").then_some("fixture incomplete coverage"), path],
        ))?;
        anyhow::ensure!(count == 1, "fixture document absent");
        storage_result(tx.commit())?;
        Ok(())
    }
}
