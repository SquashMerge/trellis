//! Independent in-memory FULL comparison for completed and unavailable semantic catch-up stages.
#![cfg(feature = "test-support")]

use super::*;
use crate::{native_ids, semantic_batch::AdmittedBatch, semantic_records as sem};

pub(in crate::store) fn enabled(store: &Store) -> bool {
    store.semantic_test_hooks().full_oracle || TEST_REJOIN_ORACLE_MISMATCH.get()
}

pub(super) fn check(
    store: &Store,
    db: &Connection,
    target: &PendingSemanticTarget,
    selection: &PendingSemanticSelection,
    producer: &sem::Producer,
    admitted: &mut AdmittedBatch,
    cancel: &CancelFlag,
) -> Result<sem::SemanticPublication> {
    let work_checkpoint = crate::semantic_import::publication_work_checkpoint();
    let result = (|| -> Result<sem::SemanticPublication> {
        check_cancel(cancel)?;
        let mut actual = crate::semantic_import::empty_publication(producer);
        crate::semantic_import::read_staged_documents(
            admitted.join_parts()?.conn,
            "",
            producer,
            &mut |document| {
                check_cancel(cancel)?;
                actual.coverage.extend(document.coverage);
                actual.provenance.extend(document.provenance);
                actual.symbols.extend(document.symbols);
                actual
                    .declaration_bindings
                    .extend(document.declaration_bindings);
                actual.references.extend(document.references);
                actual.call_bindings.extend(document.call_bindings);
                actual
                    .type_relationships
                    .extend(document.type_relationships);
                Ok(())
            },
        )?;
        // Documents the witnessed stage copies are compared as the publisher writes them.
        let source_set_id = admitted.descriptor().source_set.id.clone();
        let copied = super::copy::copied_documents(admitted.join_parts()?.conn, &source_set_id)?;
        if !copied.is_empty() {
            let destination = super::copy::CopyDestination::read(store, db, target)?;
            for document in &copied {
                let copy = super::copy::copied_document(
                    db,
                    target,
                    selection,
                    producer,
                    &destination,
                    store.payload_key_inputs().join_inputs(),
                    document,
                )?;
                actual.coverage.extend(copy.coverage);
                actual.provenance.extend(copy.provenance);
                actual.symbols.extend(copy.symbols);
                actual
                    .declaration_bindings
                    .extend(copy.declaration_bindings);
                actual.references.extend(copy.references);
                actual.call_bindings.extend(copy.call_bindings);
                actual.type_relationships.extend(copy.type_relationships);
            }
        }
        let full = full_rejoin(store, db, target, selection, producer, cancel)?;
        check_cancel(cancel)?;
        if TEST_REJOIN_ORACLE_MISMATCH.get()
            || sorted_publication_rows(&actual)? != sorted_publication_rows(&full)?
        {
            return Err(RejoinOracleMismatch.into());
        }
        Ok(full)
    })();
    crate::semantic_import::restore_publication_work(work_checkpoint);
    result
}

/// F2. The committed head's reader-visible rows for `producer` against the independent FULL
/// rejoin: a wrong writer hit, copy, projection or freshness row shows here.
pub(in crate::store) fn check_published(
    db: &Connection,
    published: IndexPin,
    producer: &sem::Producer,
    full: &sem::SemanticPublication,
) -> Result<()> {
    let snapshot = read_semantic_tuple(db, &semantic_pin_key(published), None, None, false)?;
    let provenance: Vec<_> = snapshot
        .provenance
        .into_iter()
        .filter(|p| p.producer_id == producer.id)
        .collect();
    let owned: BTreeSet<&str> = provenance.iter().map(|p| p.id.as_str()).collect();
    let actual = sem::SemanticPublication {
        producers: vec![],
        coverage: snapshot
            .coverage
            .into_iter()
            .filter(|c| c.producer_id == producer.id)
            .collect(),
        symbols: snapshot
            .symbols
            .into_iter()
            .filter(|f| owned.contains(f.provenance_id.as_str()))
            .collect(),
        declaration_bindings: snapshot
            .declaration_bindings
            .into_iter()
            .filter(|f| owned.contains(f.provenance_id.as_str()))
            .collect(),
        references: snapshot
            .references
            .into_iter()
            .filter(|f| owned.contains(f.provenance_id.as_str()))
            .collect(),
        call_bindings: snapshot
            .call_bindings
            .into_iter()
            .filter(|f| owned.contains(f.provenance_id.as_str()))
            .collect(),
        type_relationships: snapshot
            .type_relationships
            .into_iter()
            .filter(|f| owned.contains(f.provenance_id.as_str()))
            .collect(),
        provenance: provenance.clone(),
    };
    if sorted_publication_rows(&actual)?[1..] != sorted_publication_rows(full)?[1..] {
        return Err(RejoinOracleMismatch.into());
    }
    Ok(())
}

/// F3. A writer hit names a stored payload instead of writing rows; the rows the manifest
/// row now reads must equal the settled document it replaced.
pub(in crate::store) fn check_hit(
    db: &Connection,
    key: &str,
    tuple: i64,
    coverage: &sem::Coverage,
    publication: &sem::SemanticPublication,
) -> Result<()> {
    let owns = semantic_payload::owned_provenance(coverage, publication);
    let mine = |id: &str| owns.contains(id);
    let settled = sem::SemanticPublication {
        producers: vec![],
        coverage: vec![coverage.clone()],
        provenance: publication
            .provenance
            .iter()
            .filter(|p| mine(&p.id) && p.producer_id == coverage.producer_id)
            .cloned()
            .collect(),
        symbols: publication
            .symbols
            .iter()
            .filter(|f| mine(&f.provenance_id))
            .cloned()
            .collect(),
        declaration_bindings: publication
            .declaration_bindings
            .iter()
            .filter(|f| mine(&f.provenance_id))
            .cloned()
            .collect(),
        references: publication
            .references
            .iter()
            .filter(|f| mine(&f.provenance_id))
            .cloned()
            .collect(),
        call_bindings: publication
            .call_bindings
            .iter()
            .filter(|f| mine(&f.provenance_id))
            .cloned()
            .collect(),
        type_relationships: publication
            .type_relationships
            .iter()
            .filter(|f| mine(&f.provenance_id))
            .cloned()
            .collect(),
    };
    let source_set: String = db.query_row(
        "SELECT source_set_id FROM native_revisions WHERE id=?1",
        [key],
        |r| r.get(0),
    )?;
    let read = read_semantic_tuple_with_reasons(
        db,
        key,
        Some(tuple),
        None,
        true,
        None,
        true,
        source_set,
        None,
    )?;
    let actual = sem::SemanticPublication {
        producers: vec![],
        coverage: read.coverage,
        provenance: read.provenance,
        symbols: read.symbols,
        declaration_bindings: read.declaration_bindings,
        references: read.references,
        call_bindings: read.call_bindings,
        type_relationships: read.type_relationships,
    };
    if sorted_publication_rows(&actual)? != sorted_publication_rows(&settled)? {
        return Err(RejoinOracleMismatch.into());
    }
    Ok(())
}

/// An S that ended unavailable for `producer` is a mismatch whenever the independent FULL
/// rejoin of the same selection succeeds (#826): S must equal FULL, so it may not fail
/// where FULL publishes evidence.
pub(super) fn check_unavailable(
    store: &Store,
    db: &Connection,
    target: &PendingSemanticTarget,
    selection: &PendingSemanticSelection,
    producer: &sem::Producer,
    cancel: &CancelFlag,
) -> Result<()> {
    let work_checkpoint = crate::semantic_import::publication_work_checkpoint();
    let full = full_rejoin(store, db, target, selection, producer, cancel);
    crate::semantic_import::restore_publication_work(work_checkpoint);
    check_cancel(cancel)?;
    match full {
        Ok(_) => Err(RejoinOracleMismatch.into()),
        Err(_) => Ok(()),
    }
}

/// The independent, in-memory FULL rejoin of `selection` at the pending target.
fn full_rejoin(
    store: &Store,
    db: &Connection,
    target: &PendingSemanticTarget,
    selection: &PendingSemanticSelection,
    producer: &sem::Producer,
    cancel: &CancelFlag,
) -> Result<sem::SemanticPublication> {
    let destination_read = ReadRevision::new(target.pin, semantic_pin_key(target.pin));
    let selected = store.selected_raw_input_results_from_db(
        db,
        &destination_read,
        target.pin,
        false,
        false,
    )?;
    let (selected_producer, _, raw) = selected
        .into_iter()
        .find(|(selected_producer, _, _)| selected_producer.id == selection.producer_id)
        .context("FULL oracle selected producer missing")?;
    let raw = raw?;
    ensure!(
        selected_producer == *producer
            && raw.producer == *producer
            && raw.batch_id == selection.batch_id
            && raw.captured_pin == selection.captured_pin,
        "FULL oracle selected input differs from pending selection"
    );
    let captured_read = ReadRevision::new(
        selection.captured_pin,
        semantic_pin_key(selection.captured_pin),
    );
    let (captured_native, captured_sources) =
        store.retained_native_snapshot_from_db(db, &captured_read, None)?;
    let (toolchain, config, dependencies) =
        store.retained_native_components_from_db(db, &captured_read)?;
    let (destination_native, destination_sources) =
        store.retained_native_snapshot_from_db(db, &destination_read, None)?;
    ensure!(
        destination_native.revision.id == target.native_revision_id,
        "FULL oracle destination identity mismatch"
    );
    let inventory = decoded_selected_inventory(&raw)?;
    let captured = sem::CapturedComponents {
        producer_executables: BTreeMap::new(),
        artifacts: captured_artifacts(&raw),
        sources: captured_sources.clone(),
        captured_revision: captured_native.revision.clone(),
        native_toolchain: toolchain.clone(),
        native_config: config.clone(),
        native_dependencies: dependencies.clone(),
        requested_producers: vec![producer.clone()],
        toolchain: native_ids::canonical(&toolchain),
        config: native_ids::canonical(&serde_json::json!(config)),
        dependency: native_ids::canonical(&serde_json::json!(dependencies)),
        native_extraction_components: BTreeMap::new(),
    };
    let attested = authenticate_stored_batch(
        &raw,
        &inventory,
        &captured,
        &captured_native,
        &captured_sources,
        store,
        cancel,
    )?;
    ensure!(
        attested.matches(producer),
        "FULL oracle producer descriptor mismatch"
    );
    let mut basis = BTreeMap::new();
    for document in &captured_native.revision.documents {
        check_cancel(cancel)?;
        let language: sem::Language =
            serde_json::from_value(serde_json::json!(document.key.language))?;
        if producer.languages.contains(&language) {
            basis.insert(
                document.key.clone(),
                sem::SemanticBasis {
                    producer_id: producer.id.clone(),
                    producer_version: producer.version.clone(),
                    producer_hash: producer.executable_hash.clone(),
                    artifact_hash: raw.artifact_hash_for(&document.key).to_owned(),
                    language,
                    source_set_id: document.key.source_set_id.clone(),
                    revision_id: captured_native.revision.id.clone(),
                    source_manifest_hash: inventory.source_manifest_hash.clone(),
                    toolchain_hash: captured_native.revision.toolchain_hash.clone(),
                    config_hash: captured_native.revision.config_hash.clone(),
                    dependency_hash: captured_native.revision.dependency_hash.clone(),
                    lookup_dependencies: vec![],
                },
            );
        }
    }
    let full = crate::semantic_import::rejoin_inventory(
        &inventory,
        &captured_native,
        &captured_sources,
        &destination_native,
        &destination_sources,
        producer.clone(),
        &basis,
        match producer.position_encoding {
            sem::PositionEncoding::Utf8 => 1,
            sem::PositionEncoding::Utf16 => 2,
            sem::PositionEncoding::UnicodeScalar => 3,
        },
    )?;
    crate::semantic_evidence::validate_evidence_with_attested_batch(
        &full.publication,
        &destination_native,
        &captured,
        &raw,
        &inventory,
        &attested,
    )?;
    Ok(full.publication)
}
