//! Bounded staging of original associations under one exact pending target.
use super::witness::{self, OriginalRow, WitnessEligibility};
use super::*;
use crate::native_records::DocumentKey;
use crate::semantic_batch::staging::{Stage, WitnessFallback, WitnessStageWriter};
use crate::semantic_batch::{
    AdmittedBatch, LookupBudget, counters::BatchCounters, limits::BatchLimits,
};
use std::collections::VecDeque;

/// Rows (or inventory documents) per preparation step. A page stays bounded by the unit
/// expansion byte reservation; one row per step spent most of the step on fixed overhead.
const PAGE_ROWS: usize = 256;
const INVENTORY_PAGE: usize = 64;
/// Required targets looked up per preparation step.
const TARGET_PAGE: i64 = 64;

#[derive(Default)]
pub(super) struct WitnessPreparation {
    inventory: CanonicalInventoryCursor,
    documents: VecDeque<DocumentKey>,
    inventory_done: bool,
    source: Option<SourcePreparation>,
    symbols: witness::InternalSymbolCursor,
    symbols_done: bool,
    tables_ready: bool,
    target_after: i64,
    targets_done: bool,
    target_count: u64,
    /// Estimated LOCAL target stage bytes since that stage was last measured.
    lookup_unchecked: u64,
}
struct SourcePreparation {
    ord: i64,
    witness: witness::OriginalSourceWitness,
    cursor: witness::OriginalAssociationCursor,
    ordinal: i64,
}

fn reserve_page(used: &mut u64, bytes: u64, limits: &BatchLimits) -> Result<()> {
    *used = used
        .checked_add(bytes)
        .ok_or_else(|| anyhow::anyhow!("witness page overflow"))?;
    if *used > limits.unit_expansion_bytes {
        return Err(crate::scip_raw::ScipCaptureError::LimitExceeded(
            "unit expansion bytes".into(),
        )
        .into());
    }
    Ok(())
}

impl WitnessPreparation {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn advance(
        &mut self,
        db: &Connection,
        target: &PendingSemanticTarget,
        authority: &witness::SourceAuthority,
        destination: &NativeReader<'_>,
        admitted: &mut AdmittedBatch,
        target_stage: &mut Stage,
        limits: &BatchLimits,
        counters: &mut BatchCounters,
        lookup_budget: &mut LookupBudget,
        cancel: &CancelFlag,
    ) -> Result<bool> {
        check_cancel(cancel)?;
        lookup_budget.charge(1)?;
        let budget = admitted.stage().budget(limits);
        let conn = admitted.stage_mut().conn()?;
        if !self.tables_ready {
            self.tables_ready = true;
            conn.execute_batch("CREATE TABLE IF NOT EXISTS witness_required_targets(id INTEGER PRIMARY KEY,language TEXT NOT NULL,path TEXT NOT NULL,syntax_id TEXT NOT NULL,UNIQUE(language,path,syntax_id));
            CREATE TABLE IF NOT EXISTS witness_target_status(rev INTEGER NOT NULL,language TEXT NOT NULL,path TEXT NOT NULL,syntax_id BLOB NOT NULL,n INTEGER NOT NULL,named INTEGER NOT NULL,name_ok INTEGER NOT NULL,method INTEGER NOT NULL,UNIQUE(rev,language,path,syntax_id,n));")?;
        }
        if let Some(source) = self.source.as_mut() {
            let mut used = 0;
            let page = witness::original_document_page(
                db,
                authority,
                &source.witness,
                &mut source.cursor,
                PAGE_ROWS,
                &mut |bytes| reserve_page(&mut used, bytes, limits),
            )?;
            for row in &page.rows {
                stage_targets(
                    conn,
                    &source.witness.document,
                    row,
                    limits,
                    &mut self.target_count,
                )?;
            }
            let mut writer = WitnessStageWriter::new(admitted.stage_mut(), limits, counters)?;
            for row in &page.rows {
                writer.stage_original_row(source.ord, source.ordinal, row)?;
                source.ordinal += 1;
            }
            if page.done {
                writer.finish_source_witness(source.ord)?;
                self.source = None;
            }
            return Ok(false);
        }
        if let Some(document) = self.documents.pop_front() {
            let decision = witness::original_document_begin(db, authority, target, &document)?;
            let ord: Option<i64> = conn
                .prepare_cached("SELECT ord FROM sources WHERE language=?1 AND path=?2")?
                .query_row(params![document.language, document.path], |r| r.get(0))
                .optional()?;
            match (ord, decision) {
                (Some(ord), WitnessEligibility::Witness(witness))
                    if (super::copy::CopyFilter {
                        target: target.clone(),
                        captured_pin: authority.captured_pin,
                        producer: authority.producer.id.clone(),
                        batch_id: authority.batch_id,
                        join_inputs: authority.join_inputs.clone(),
                    })
                    .copies(db, &document)? =>
                {
                    #[cfg(feature = "test-support")]
                    {
                        crate::semantic_import::note_reused_document(&witness.document);
                        crate::semantic_import::note_copied_document(&witness.document);
                    }
                    WitnessStageWriter::new(admitted.stage_mut(), limits, counters)?
                        .stage_copied_source(ord, witness.as_ref())?;
                }
                (Some(ord), WitnessEligibility::Witness(witness)) => {
                    #[cfg(feature = "test-support")]
                    crate::semantic_import::note_reused_document(&witness.document);
                    WitnessStageWriter::new(admitted.stage_mut(), limits, counters)?
                        .stage_source_witness(ord, witness.as_ref())?;
                    self.source = Some(SourcePreparation {
                        ord,
                        witness: *witness,
                        cursor: witness::OriginalAssociationCursor::default(),
                        ordinal: 0,
                    });
                }
                (Some(ord), WitnessEligibility::ChangedBytes(coverage)) => {
                    let mut writer =
                        WitnessStageWriter::new(admitted.stage_mut(), limits, counters)?;
                    writer.stage_witness_coverage(ord, &coverage, "changedBytes")?;
                    writer.finish_source_witness(ord)?;
                }
                (None, WitnessEligibility::NewDocument | WitnessEligibility::Uncovered(_)) => {}
                (Some(ord), WitnessEligibility::Uncovered(coverage)) => {
                    let mut writer =
                        WitnessStageWriter::new(admitted.stage_mut(), limits, counters)?;
                    writer.stage_witness_coverage(ord, &coverage, "newDocument")?;
                    writer.finish_source_witness(ord)?;
                }
                (_, WitnessEligibility::Full(reason)) => return Err(WitnessFallback(reason).into()),
                _ => return Err(WitnessFallback("witness_source_inventory_mismatch").into()),
            }
            return Ok(false);
        }
        if !self.inventory_done {
            let page = destination.canonical_inventory_page(&mut self.inventory, INVENTORY_PAGE)?;
            self.inventory_done = page.is_empty();
            self.documents
                .extend(page.into_iter().map(|document| document.key));
            return Ok(false);
        }
        if !self.symbols_done {
            let mut used = 0;
            let page = witness::internal_symbols_page(
                db,
                authority,
                &mut self.symbols,
                PAGE_ROWS,
                &mut |bytes| reserve_page(&mut used, bytes, limits),
            )?;
            let mut writer = WitnessStageWriter::new(admitted.stage_mut(), limits, counters)?;
            for key in &page {
                writer.stage_internal_symbol(key)?;
            }
            if self.symbols.done {
                writer.finish_internal_symbols()?;
                self.symbols_done = true;
            }
            return Ok(false);
        }
        if !self.targets_done {
            // A page of required targets per step; each target is still one lookup step.
            let page: Vec<(i64, String, String, String)> = conn.prepare_cached("SELECT id,language,path,syntax_id FROM witness_required_targets WHERE id>?1 ORDER BY id LIMIT ?2")?.query_map(
                params![self.target_after, TARGET_PAGE], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
            let more = !page.is_empty();
            for (index, (id, language, path, syntax_id)) in page.into_iter().enumerate() {
                if index > 0 {
                    lookup_budget.charge(1)?;
                }
                let key = DocumentKey {
                    source_set_id: authority.source_set_id.clone(),
                    language,
                    path,
                };
                for (rev, pin) in [(0, authority.captured_pin), (1, target.pin)] {
                    let lookup = super::local_targets::lookup(
                        db,
                        pin,
                        &key,
                        &syntax_id,
                        target_stage,
                        limits,
                        &mut self.lookup_unchecked,
                        cancel,
                    )?;
                    #[cfg(feature = "test-support")]
                    {
                        crate::semantic_import::note_local_target_work(0, 0, lookup.rows);
                        if lookup.read {
                            crate::semantic_import::note_local_target_lookup(&key);
                        }
                    }
                    if let Some(row) = lookup.declaration {
                        let encoded = id_codec::encode_sid(&syntax_id)
                            .context("invalid witnessed target syntax ID")?;
                        let named = row.name_range.is_some();
                        crate::semantic_batch::staging::charge_witness_row(conn, limits)?;
                        conn.prepare_cached(
                            "INSERT INTO witness_target_status VALUES(?1,?2,?3,?4,0,?5,?6,?7)",
                        )?
                        .execute(params![
                            rev,
                            key.language,
                            key.path,
                            encoded,
                            named,
                            named && row.name.is_some(),
                            row.kind == "method"
                        ])?;
                    }
                }
                self.target_after = id;
            }
            // Lookups charge the LOCAL target stage per MiB; each page, the last one and the
            // empty completion page also measure it, after writing the slice's open batch so
            // the file lengths include its pages, as well as the admitted stage.
            target_stage.check_flushed(limits)?;
            self.lookup_unchecked = 0;
            if more {
                budget.check()?;
                return Ok(false);
            }
            self.targets_done = true;
        }
        budget.check()?;
        Ok(true)
    }
}

fn stage_targets(
    conn: &Connection,
    source: &DocumentKey,
    row: &OriginalRow,
    limits: &BatchLimits,
    count: &mut u64,
) -> Result<()> {
    use crate::semantic_evidence::Target;
    let target = |target: &Target, count: &mut u64| -> Result<()> {
        if let Target::Internal {
            document,
            syntax_id,
            ..
        } = target
        {
            stage_target(conn, document, syntax_id, limits, count)?;
        }
        Ok(())
    };
    match row {
        OriginalRow::SymbolTarget { target: value, .. }
        | OriginalRow::ReferenceCandidate { target: value, .. }
        | OriginalRow::CallCandidate { target: value, .. }
        | OriginalRow::CallDispatch { target: value, .. } => target(value, count)?,
        OriginalRow::Reference(reference) => {
            stage_target(conn, source, &reference.owner_syntax_id, limits, count)?;
            if let Some(value) = &reference.declared_target {
                target(value, count)?;
            }
        }
        OriginalRow::CallBinding(call) => {
            if let Some(value) = &call.declared_target {
                target(value, count)?;
            }
        }
        OriginalRow::DeclarationBinding(declaration) => {
            if let Some(syntax_id) = &declaration.syntax_id {
                stage_target(conn, &declaration.join.document, syntax_id, limits, count)?;
            }
        }
        OriginalRow::DeclarationCandidate { id, .. } => {
            stage_target(conn, source, id, limits, count)?;
        }
        OriginalRow::TypeRelationship { row, .. } => {
            target(&row.source, count)?;
            target(&row.target, count)?;
        }
        _ => {}
    }
    Ok(())
}

fn stage_target(
    conn: &Connection,
    document: &DocumentKey,
    syntax_id: &str,
    limits: &BatchLimits,
    count: &mut u64,
) -> Result<()> {
    let present: bool = conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM witness_required_targets WHERE language=?1 AND path=?2 AND syntax_id=?3)")?.query_row(
        params![document.language,document.path,syntax_id], |r|r.get(0))?;
    if !present {
        if *count >= limits.candidates {
            return Err(
                crate::scip_raw::ScipCaptureError::LimitExceeded("candidates".into()).into(),
            );
        }
        crate::semantic_batch::staging::charge_witness_row(conn, limits)?;
        conn.prepare_cached(
            "INSERT INTO witness_required_targets(language,path,syntax_id) VALUES(?1,?2,?3)",
        )?
        .execute(params![document.language, document.path, syntax_id])?;
        *count += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_required_targets_share_the_existing_stage_row_budget() {
        let parent = tempfile::tempdir().unwrap();
        let mut stage = Stage::create(parent.path(), "witness-targets", "test").unwrap();
        let conn = stage.conn().unwrap();
        conn.execute_batch("INSERT INTO witness_state(singleton) VALUES(1);
            CREATE TABLE witness_required_targets(id INTEGER PRIMARY KEY,language TEXT,path TEXT,syntax_id TEXT,UNIQUE(language,path,syntax_id));").unwrap();
        let limits = BatchLimits {
            staged_rows: 1,
            ..Default::default()
        };
        let document = DocumentKey {
            source_set_id: "set".into(),
            language: "javascript".into(),
            path: "a.js".into(),
        };
        let mut count = 0;
        stage_target(conn, &document, "first", &limits, &mut count).unwrap();
        stage_target(conn, &document, "first", &limits, &mut count).unwrap();
        assert_eq!(count, 1);
        let error = stage_target(conn, &document, "second", &limits, &mut count).unwrap_err();
        assert!(error.is::<crate::scip_raw::ScipCaptureError>());
        assert_eq!(count, 1);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM witness_required_targets", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
            1
        );
    }
}
