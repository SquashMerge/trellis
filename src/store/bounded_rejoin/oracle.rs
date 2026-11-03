//! Independent in-memory FULL oracle for test-support bounded rejoin checks.
#![cfg(feature = "test-support")]

use super::super::*;
use crate::{native_ids, semantic_evidence as sem};

pub(super) fn check(
    store: &Store,
    tx: &Connection,
    input: &SelectedNativeInput,
    pin: IndexPin,
    actual: &sem::SemanticPublication,
    cancel: &CancelFlag,
) -> Result<()> {
    let work_checkpoint = crate::semantic_import::publication_work_checkpoint();
    let result = (|| -> Result<()> {
        check_cancel(cancel)?;
        let selected_read = ReadRevision::new(pin, semantic_pin_key(pin));
        let full_selected = store.selected_native_inputs_from_db(tx, &selected_read, pin, None)?;
        let full_input = full_selected
            .iter()
            .find(|row| row.producer.id == input.producer.id)
            .context("FULL oracle selected producer missing")?;
        let raw = full_input
            .raw
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        let (captured_native, captured_sources, (toolchain, config, dependencies)) = full_input
            .captured
            .as_ref()
            .map_err(|error| anyhow::anyhow!("{error:#}"))?;
        let inventory = decoded_selected_inventory(raw)?;
        let captured = sem::CapturedComponents {
            producer_executables: BTreeMap::new(),
            artifacts: captured_artifacts(raw),
            sources: captured_sources.clone(),
            captured_revision: captured_native.revision.clone(),
            native_toolchain: toolchain.clone(),
            native_config: config.clone(),
            native_dependencies: dependencies.clone(),
            requested_producers: vec![input.producer.clone()],
            toolchain: native_ids::canonical(toolchain),
            config: native_ids::canonical(&serde_json::json!(config)),
            dependency: native_ids::canonical(&serde_json::json!(dependencies)),
            native_extraction_components: BTreeMap::new(),
        };
        let attested = authenticate_stored_batch(
            raw,
            &inventory,
            &captured,
            captured_native,
            captured_sources,
            &store.identity,
            cancel,
        )?;
        ensure!(
            attested.matches(&input.producer),
            "FULL oracle producer descriptor mismatch"
        );
        let mut basis = BTreeMap::new();
        for document in &captured_native.revision.documents {
            check_cancel(cancel)?;
            let language: sem::Language =
                serde_json::from_value(serde_json::json!(document.key.language))?;
            if input.producer.languages.contains(&language) {
                basis.insert(
                    document.key.clone(),
                    sem::SemanticBasis {
                        producer_id: input.producer.id.clone(),
                        producer_version: input.producer.version.clone(),
                        producer_hash: input.producer.executable_hash.clone(),
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
        let (destination_native, destination_sources) =
            store.retained_native_snapshot_from_db(tx, &selected_read, None)?;
        let full = crate::semantic_import::rejoin_inventory(
            &inventory,
            captured_native,
            captured_sources,
            &destination_native,
            &destination_sources,
            input.producer.clone(),
            &basis,
            match input.producer.position_encoding {
                sem::PositionEncoding::Utf8 => 1,
                sem::PositionEncoding::Utf16 => 2,
                sem::PositionEncoding::UnicodeScalar => 3,
            },
        )?;
        check_cancel(cancel)?;
        if TEST_REJOIN_ORACLE_MISMATCH.get()
            || sorted_publication_rows(actual)? != sorted_publication_rows(&full.publication)?
        {
            return Err(RejoinOracleMismatch.into());
        }
        Ok(())
    })();
    crate::semantic_import::restore_publication_work(work_checkpoint);
    result
}
