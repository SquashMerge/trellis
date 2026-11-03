//! Bounded retained semantic catch-up inside the native writer transaction.
use super::*;
use crate::semantic_batch::{BatchCounters, LookupBudget, StageOwner};
use crate::semantic_import::{DestinationMutations, read_staged_documents};
#[cfg(feature = "test-support")]
mod oracle;

pub(super) fn publish(
    store: &Store,
    tx: &Connection,
    selected: &[SelectedNativeInput],
    _native: &crate::native_evidence::Artifact,
    pin: IndexPin,
    cancel: &CancelFlag,
    semantic_document_written: &mut impl FnMut(&Connection) -> Result<()>,
) -> Result<()> {
    let key = semantic_pin_key(pin);
    let destination_read = ReadRevision::new(pin, key.clone());
    let destination = NativeReader {
        store,
        db: tx,
        selected: &destination_read,
    };
    let limits = batch_publication_limits();
    for input in selected {
        check_cancel(cancel)?;
        if let Err(error) = &input.raw
            && rejoin_storage_failure(error)
        {
            return Err(SelectedIntegrity(format!(
                "incompatible_index: selected batch unavailable: {error:#}"
            ))
            .into());
        }
        tx.execute_batch("SAVEPOINT bounded_semantic_rejoin")?;
        let attempt = (|| -> Result<()> {
            let raw = input
                .raw
                .as_ref()
                .map_err(|error| anyhow::anyhow!("{error:#}"))?;
            let captured_read =
                ReadRevision::new(raw.captured_pin, semantic_pin_key(raw.captured_pin));
            let captured = NativeReader {
                store,
                db: tx,
                selected: &captured_read,
            };
            let mut counters = BatchCounters::default();
            let request = format!("rejoin-{}", uuid::Uuid::new_v4());
            let incarnation = uuid::Uuid::new_v4().to_string();
            let mut admitted = crate::semantic_batch::retained::stage_retained_batch(
                tx,
                &captured,
                raw.batch_id,
                &store.semantic_stage_parent()?,
                StageOwner {
                    request_id: &request,
                    incarnation: &incarnation,
                },
                &limits,
                &mut counters,
                cancel,
            )?;
            ensure!(
                admitted.producer().producer() == input.producer,
                "selected batch producer descriptor mismatch"
            );
            if raw.descriptor.trust_origin == crate::scip_admission::REFERENCE_TRUST_ORIGIN
                && raw.captures.len() == 1
                && !witness_rejoin::has_original(tx, raw)?
            {
                let mut budget = LookupBudget::new(&limits);
                crate::semantic_import::join_staged_with_readers(
                    &mut admitted,
                    &captured,
                    &captured,
                    raw.captured_pin,
                    &limits,
                    &mut budget,
                    &mut counters,
                    DestinationMutations::default(),
                    cancel,
                )?;
                read_staged_documents(
                    admitted.join_parts()?.conn,
                    "",
                    &input.producer,
                    &mut |document| {
                        check_cancel(cancel)?;
                        captured_witness::write_document(tx, raw.batch_id, document)
                    },
                )?;
                captured_witness::complete(tx, raw.batch_id)?;
            }
            let mut budget = LookupBudget::new(&limits);
            crate::semantic_import::join_staged_with_readers(
                &mut admitted,
                &captured,
                &destination,
                pin,
                &limits,
                &mut budget,
                &mut counters,
                DestinationMutations::default(),
                cancel,
            )?;
            #[cfg(feature = "test-support")]
            let mut oracle_rows = (std::env::var_os("TRELLIS_TEST_LOCAL_FULL_ORACLE").is_some()
                || TEST_REJOIN_ORACLE_MISMATCH.get())
            .then(|| crate::semantic_import::empty_publication(&input.producer));
            read_staged_documents(
                admitted.join_parts()?.conn,
                "",
                &input.producer,
                &mut |document| {
                    check_cancel(cancel)?;
                    write_import_coverage(tx, &key, &document)?;
                    #[cfg(feature = "test-support")]
                    if let Some(rows) = oracle_rows.as_mut() {
                        rows.coverage.extend(document.coverage);
                        rows.provenance.extend(document.provenance);
                        rows.symbols.extend(document.symbols);
                        rows.declaration_bindings
                            .extend(document.declaration_bindings);
                        rows.references.extend(document.references);
                        rows.call_bindings.extend(document.call_bindings);
                        rows.type_relationships.extend(document.type_relationships);
                    }
                    semantic_document_written(tx)
                },
            )?;
            #[cfg(feature = "test-support")]
            if let Some(rows) = oracle_rows.as_ref() {
                oracle::check(store, tx, input, pin, rows, cancel)?;
            }
            Ok(())
        })();
        check_cancel(cancel)?;
        match attempt {
            Ok(()) => tx.execute_batch("RELEASE bounded_semantic_rejoin")?,
            Err(error) => {
                tx.execute_batch(
                    "ROLLBACK TO bounded_semantic_rejoin; RELEASE bounded_semantic_rejoin",
                )?;
                if rejoin_storage_failure(&error) {
                    return Err(rejoin_storage_error(error));
                }
                #[cfg(feature = "test-support")]
                if error.is::<RejoinOracleMismatch>() {
                    return Err(error);
                }
                let diagnostic: String = format!("selected raw capture unavailable: {error:#}")
                    .chars()
                    .take(512)
                    .collect();
                let requested: BTreeSet<_> = input.requested_documents.iter().collect();
                let mut inventory = destination.inventory()?;
                while let Some(document) = inventory.next_document()? {
                    let language: crate::semantic_evidence::Language =
                        serde_json::from_value(serde_json::json!(document.key.language))?;
                    if !input.producer.languages.contains(&language) {
                        continue;
                    }
                    let selected = requested.contains(&document.key);
                    let mut publication =
                        crate::semantic_import::empty_publication(&input.producer);
                    publication
                        .coverage
                        .push(crate::semantic_evidence::Coverage {
                            producer_id: input.producer.id.clone(),
                            language,
                            source_set_id: document.key.source_set_id.clone(),
                            document_path: document.key.path.clone(),
                            revision_id: document.revision_id.clone(),
                            requested: selected,
                            selected,
                            state: if selected {
                                crate::semantic_evidence::CoverageState::Failed
                            } else {
                                crate::semantic_evidence::CoverageState::NotRequested
                            },
                            supported_roles: vec![],
                            observed_roles: vec![],
                            diagnostic: selected.then(|| diagnostic.clone()),
                            reasons: None,
                        });
                    write_import_coverage(tx, &key, &publication)?;
                    semantic_document_written(tx)?;
                }
            }
        }
        #[cfg(feature = "test-support")]
        if let Some(file) = std::env::var_os("TRELLIS_TEST_LOCAL_WORK_COUNTS_FILE") {
            let mut work = crate::semantic_import::publication_work();
            work["indexPin"] = serde_json::to_value(pin)?;
            work["producerIds"] = serde_json::to_value(
                selected
                    .iter()
                    .map(|input| &input.producer.id)
                    .collect::<Vec<_>>(),
            )?;
            std::fs::write(file, serde_json::to_vec(&work)?)?;
        }
    }
    Ok(())
}
