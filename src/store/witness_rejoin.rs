//! The witnessed rejoin of a LOCAL publication (Decision 0002): a document whose version is the
//! captured one reuses its admitted source associations, and every document is rejoined,
//! validated and written on its own. Only one document's rows are in memory at a time, so the
//! cost of an edit does not include materializing the whole publication, as FULL must.
use super::*;
use crate::native_evidence::{Artifact, DocumentKey};
use crate::semantic_evidence as sem;
use crate::semantic_import::{
    WitnessScope, WitnessedRejoin, empty_publication, manifest_hash,
    original_document_associations, rejoin_document_with_witness, required_original_targets,
};

/// What one selected producer's witnessed rejoin stands on.
pub(super) struct WitnessedInput<'a> {
    pub tx: &'a Connection,
    pub key: &'a str,
    pub raw: &'a SelectedRawBatch,
    pub producer: &'a sem::Producer,
    pub original_native: &'a Artifact,
    pub original_sources: &'a BTreeMap<DocumentKey, Vec<u8>>,
    pub destination: &'a Artifact,
    pub destination_sources: &'a BTreeMap<DocumentKey, Vec<u8>>,
    pub reusable: &'a BTreeSet<DocumentKey>,
    pub pin: IndexPin,
    pub toolchain: &'a serde_json::Value,
    pub config: &'a [serde_json::Value],
    pub dependencies: &'a [serde_json::Value],
    pub changed: &'a crate::semantic_import::MeasuredFacts<'a>,
    pub cancel: &'a CancelFlag,
}

/// The revision that took the selected batch's original import.
fn original_import_key(tx: &Connection, raw: &SelectedRawBatch) -> Result<Option<String>> {
    tx.query_row(
        "SELECT s.revision_id FROM semantic_revision_raw_selections s
         JOIN native_revisions r ON r.id=s.revision_id
         WHERE s.batch_id=?1 AND s.producer_id=?2 AND s.captured_pin=?3
           AND r.native_revision_id=?4 AND r.source_set_id=?5
         ORDER BY r.published_index_revision LIMIT 1",
        params![
            raw.batch_id,
            raw.producer_id,
            semantic_pin_key(raw.captured_pin),
            raw.captured_revision,
            raw.source_set_id
        ],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// The admitted tuple of `document`, or `None` when it has none.
fn original_document_tuple(
    tx: &Connection,
    original: &str,
    producer_id: &str,
    document: &DocumentKey,
) -> Result<Option<sem::SemanticSnapshot>> {
    let tuple: Option<i64> = tx
        .prepare_cached(
            "SELECT tuple_id FROM semantic_coverage
             WHERE revision_id=?1 AND producer_id=?2 AND language=?3 AND path=?4",
        )?
        .query_row(
            params![original, producer_id, document.language, document.path],
            |row| row.get(0),
        )
        .optional()?;
    tuple
        .map(|tuple| read_semantic_tuple(tx, original, Some(tuple), None, true))
        .transpose()
}

/// The claims of `document` in `capture_id`, without their canonical bytes.
fn document_claims(
    tx: &Connection,
    capture_id: i64,
    artifact_hash: &str,
    document: &DocumentKey,
) -> Result<Vec<crate::scip_raw::RawProofIdentity>> {
    tx.prepare_cached(
        "SELECT proof_id,record_kind,fact_digest FROM semantic_raw_proofs
         WHERE capture_id=?1 AND language=?2 AND path=?3",
    )?
    .query_map(
        params![capture_id, document.language, document.path],
        |row| {
            Ok(crate::scip_raw::RawProofIdentity {
                proof_id: row.get(0)?,
                record_kind: row.get(1)?,
                fact_digest: row.get(2)?,
                language: document.language.clone(),
                path: document.path.clone(),
                artifact_hash: artifact_hash.to_owned(),
            })
        },
    )?
    .collect::<rusqlite::Result<_>>()
    .map_err(Into::into)
}

/// Every captured declaration and the symbols that name it, from the admitted tuples.
fn witness_scope(
    tx: &Connection,
    original: &str,
    producer_id: &str,
    source_set_id: &str,
    destination_manifest_hash: String,
) -> Result<WitnessScope> {
    let mut scope = WitnessScope {
        internal_symbols: BTreeSet::new(),
        declarations: BTreeSet::new(),
        destination_manifest_hash,
    };
    let mut statement = tx.prepare(
        "SELECT b.syntax_id,c.language,c.path,s.symbol
         FROM semantic_coverage c
         JOIN semantic_declaration_bindings b ON b.tuple_id=c.tuple_id
         LEFT JOIN semantic_declaration_binding_symbols s ON s.binding_id=b.binding_id
         WHERE c.revision_id=?1 AND c.producer_id=?2 AND b.syntax_id IS NOT NULL",
    )?;
    let mut rows = statement.query(params![original, producer_id])?;
    while let Some(row) = rows.next()? {
        let syntax_id: Vec<u8> = row.get(0)?;
        scope.declarations.insert((
            DocumentKey {
                source_set_id: source_set_id.to_owned(),
                language: row.get(1)?,
                path: row.get(2)?,
            },
            id_codec::decode_sid(&syntax_id).context(SelectedIntegrity(
                "incompatible_index: corrupt semantic declaration ID".into(),
            ))?,
        ));
        if let Some(symbol) = row.get::<_, Option<String>>(3)? {
            scope.internal_symbols.insert(symbol);
        }
    }
    Ok(scope)
}

/// `document` reuses its admitted tuple, or the rejoin leaves for FULL naming the unmet
/// requirement.
fn document_witness(
    input: &WitnessedInput<'_>,
    original_key: &str,
    document: &DocumentKey,
    captured: &crate::native_evidence::Document,
    current: &crate::native_evidence::Document,
    excluded: bool,
    claimed: bool,
) -> Result<sem::WitnessedDocument> {
    let raw = input.raw;
    let capture = &raw.captures[0];
    let admitted = raw
        .documents
        .get(&(document.language.clone(), document.path.clone()));
    if admitted.is_some() == excluded {
        return witness_unavailable("admission_differs_from_coverage");
    }
    let (source_hash, wire_digest) = admitted
        .map(|(hash, digest, _)| (hash.as_str(), Some(digest.clone())))
        .unwrap_or((&captured.content_hash, None));
    let Some(captured_version) = native_version_at(input.tx, raw.captured_pin, document)? else {
        return witness_unavailable("captured_version_missing");
    };
    let Some(current_version) = native_version_at(input.tx, input.pin, document)? else {
        return witness_unavailable("current_version_missing");
    };
    if captured_version != current_version
        || captured.content_hash != source_hash
        || current.content_hash != captured.content_hash
        || wire_digest.as_ref().is_some_and(|digest| {
            digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return witness_unavailable("reused_document_differs_from_capture");
    }
    let Some(selected) =
        original_document_tuple(input.tx, original_key, &raw.producer_id, document)?
    else {
        return witness_unavailable("original_tuple_missing");
    };
    let [coverage] = selected.coverage.as_slice() else {
        return witness_unavailable("original_tuple_differs_from_capture");
    };
    let fact_bearing = matches!(
        coverage.state,
        sem::CoverageState::Complete | sem::CoverageState::Partial
    );
    if (!excluded && (!coverage.requested || !coverage.selected || !fact_bearing))
        || (excluded && (fact_bearing || claimed || !selected.provenance.is_empty()))
        // A row dropped at admission for an absent target can return with that target; only
        // the raw claim can tell, so FULL decides.
        || coverage
            .reasons
            .as_ref()
            .is_some_and(|reasons| reasons.omitted)
        || selected.provenance.iter().any(|row| {
            row.revision_id != raw.captured_revision
                || row.content_hash != source_hash
                || row.basis.as_ref().is_none_or(|basis| {
                    basis.revision_id != raw.captured_revision
                        || basis.artifact_hash != capture.artifact_hash
                        || basis.producer_id != raw.producer_id
                        || basis.source_set_id != raw.source_set_id
                })
        })
    {
        return witness_unavailable("original_tuple_differs_from_capture");
    }
    Ok(sem::WitnessedDocument::new(
        source_hash.to_owned(),
        wire_digest,
        captured_version,
        current_version,
        selected,
    ))
}

/// Whether a proof of `dropped` claims the span of a proof of `fused`: its row was fused from
/// both, and the row alone cannot say what the surviving claim keeps.
fn shares_a_span(
    tx: &Connection,
    capture_id: i64,
    dropped: &BTreeSet<String>,
    fused: &BTreeSet<String>,
) -> Result<bool> {
    let range = |proof: &String| -> Result<serde_json::Value> {
        let bytes: Vec<u8> = tx
            .prepare_cached(
                "SELECT canonical_bytes FROM semantic_raw_proofs
                 WHERE capture_id=?1 AND proof_id=?2",
            )?
            .query_row(params![capture_id, proof], |row| row.get(0))?;
        let envelope: serde_json::Value = serde_json::from_slice(&bytes).context(
            SelectedIntegrity("incompatible_index: corrupt raw envelope".into()),
        )?;
        Ok(envelope["raw"]["range"].clone())
    };
    let fused = fused.iter().map(range).collect::<Result<Vec<_>>>()?;
    for proof in dropped {
        if fused.contains(&range(proof)?) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Looks up the current declarations `keys` name, once each. An absent one stays absent from
/// `targets`, which is how the rejoin learns it.
fn current_targets(
    input: &WitnessedInput<'_>,
    validator: &sem::WitnessValidator<'_>,
    keys: BTreeSet<(DocumentKey, String)>,
    targets: &mut BTreeMap<(DocumentKey, String), sem::TargetDeclaration>,
    absent: &mut BTreeSet<(DocumentKey, String)>,
) -> Result<()> {
    for key in keys {
        if targets.contains_key(&key) || absent.contains(&key) {
            continue;
        }
        match targeted_current_declaration(input.tx, input.pin, &key.0, &key.1)? {
            Some(target) => {
                validator
                    .target(&target)
                    .map_err(|_| WitnessFallback("witness_validation_failed"))?;
                targets.insert(key, target);
            }
            None => {
                absent.insert(key);
            }
        }
    }
    Ok(())
}

/// A header of `artifact`: everything but its facts and its document list.
fn document_header(artifact: &Artifact) -> Artifact {
    Artifact {
        producer: artifact.producer.clone(),
        source_set: artifact.source_set.clone(),
        revision: crate::native_evidence::Revision {
            documents: vec![],
            ..artifact.revision.clone()
        },
        coverage: vec![],
        provenance: vec![],
        declarations: vec![],
        calls: vec![],
        control_regions: vec![],
        references: vec![],
        capability_reports: vec![],
        search_cards: vec![],
    }
}

/// Rejoins, validates and writes every document of `input.producer` into the destination
/// revision `input.key`. Anything that keeps a document from reusing its admitted association
/// is a [`WitnessFallback`], which the caller's savepoint turns into FULL. Under the test
/// oracle, `collected` receives every document's rows.
pub(super) fn publish_with_witness(
    input: &WitnessedInput<'_>,
    #[cfg(feature = "test-support")] mut collected: Option<&mut sem::SemanticPublication>,
) -> Result<()> {
    use crate::native_ids;
    let WitnessedInput {
        tx,
        raw,
        producer,
        original_native,
        destination,
        reusable,
        ..
    } = *input;
    if reusable.is_empty()
        || destination.producer != original_native.producer
        || destination.revision.toolchain_hash != original_native.revision.toolchain_hash
        || destination.revision.dependency_hash != original_native.revision.dependency_hash
        || raw.captured_revision != original_native.revision.id
        || raw.source_manifest_hash != manifest_hash(&original_native.revision)
        || original_native.revision.source_set_id != destination.revision.source_set_id
    {
        return witness_unavailable("destination_differs_from_capture");
    }
    let Some(original_key) = original_import_key(tx, raw)? else {
        return witness_unavailable("original_import_missing");
    };
    let header_inventory = crate::scip_raw::ProofInventory {
        artifact_hashes: raw
            .captures
            .iter()
            .map(|capture| capture.artifact_hash.clone())
            .collect(),
        source_manifest_hash: raw.source_manifest_hash.clone(),
        exclusions: vec![],
        capped_documents: Default::default(),
        claims: vec![],
    };
    let Some(attested) =
        attest_selected_batch_fast(raw, producer, original_native, &header_inventory)?
    else {
        return witness_unavailable("batch_attestation_unavailable");
    };
    let capture = &raw.captures[0];
    let mut basis = BTreeMap::new();
    for doc in &original_native.revision.documents {
        let language: sem::Language = serde_json::from_value(serde_json::json!(doc.key.language))?;
        if producer.languages.contains(&language) {
            basis.insert(
                doc.key.clone(),
                sem::SemanticBasis {
                    producer_id: producer.id.clone(),
                    producer_version: producer.version.clone(),
                    producer_hash: producer.executable_hash.clone(),
                    artifact_hash: capture.artifact_hash.clone(),
                    language,
                    source_set_id: doc.key.source_set_id.clone(),
                    revision_id: original_native.revision.id.clone(),
                    source_manifest_hash: raw.source_manifest_hash.clone(),
                    toolchain_hash: original_native.revision.toolchain_hash.clone(),
                    config_hash: original_native.revision.config_hash.clone(),
                    dependency_hash: original_native.revision.dependency_hash.clone(),
                    lookup_dependencies: vec![],
                },
            );
        }
    }
    let changed_sources: BTreeMap<_, _> = input
        .destination_sources
        .iter()
        .filter(|(doc, _)| !reusable.contains(*doc))
        .map(|(doc, bytes)| (doc.clone(), bytes.clone()))
        .collect();
    let mut measured = destination.clone();
    measured.coverage.retain(|row| {
        changed_sources.keys().any(|doc| {
            doc.source_set_id == row.source_set_id
                && doc.language == row.language
                && doc.path == row.document_path
        })
    });
    measured
        .provenance
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .declarations
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .references
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .calls
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .control_regions
        .retain(|row| changed_sources.contains_key(&row.document));
    measured
        .capability_reports
        .retain(|row| changed_sources.contains_key(&row.document));
    let captured = sem::CapturedComponents {
        producer_executables: BTreeMap::new(),
        artifacts: BTreeMap::new(),
        sources: input
            .original_sources
            .iter()
            .filter(|(doc, _)| !reusable.contains(*doc))
            .map(|(doc, bytes)| (doc.clone(), bytes.clone()))
            .collect(),
        captured_revision: original_native.revision.clone(),
        native_toolchain: input.toolchain.clone(),
        native_config: input.config.to_vec(),
        native_dependencies: input.dependencies.to_vec(),
        requested_producers: vec![producer.clone()],
        toolchain: native_ids::canonical(input.toolchain),
        config: native_ids::canonical(&serde_json::json!(input.config)),
        dependency: native_ids::canonical(&serde_json::json!(input.dependencies)),
        native_extraction_components: BTreeMap::new(),
    };
    let validator = sem::WitnessValidator::new(
        producer,
        &measured,
        &captured,
        sem::WitnessedRaw {
            producer_id: &raw.producer_id,
            source_set_id: &raw.source_set_id,
            captured_revision: &raw.captured_revision,
            artifact_hash: &capture.artifact_hash,
            binary_bytes: &capture.binary_bytes,
            source_manifest_hash: &raw.source_manifest_hash,
        },
        &attested,
    )
    .map_err(|error| witness_validation_failed(&error))?;
    // The tuple writers' statements, each target table's included, stay prepared.
    tx.set_prepared_statement_cache_capacity(64);
    let scope = witness_scope(
        tx,
        &original_key,
        &raw.producer_id,
        &raw.source_set_id,
        manifest_hash(&destination.revision),
    )?;
    let facts = input.changed.over_manifest(destination);
    let captured_documents: BTreeMap<_, _> = original_native
        .revision
        .documents
        .iter()
        .map(|doc| (&doc.key, doc))
        .collect();
    let mut captured_header = document_header(original_native);
    let mut destination_header = document_header(destination);
    let mut targets = BTreeMap::new();
    let mut absent = BTreeSet::new();
    // The producer rows, which every coverage tuple below refers to.
    let mut descriptor = captured.clone();
    descriptor.requested_producers.clear();
    write_import_semantic(tx, input.key, &empty_publication(producer), &descriptor)?;
    for doc in &destination.revision.documents {
        let language: sem::Language = serde_json::from_value(serde_json::json!(doc.key.language))?;
        if !producer.languages.contains(&language) {
            continue;
        }
        check_cancel(input.cancel)?;
        let excluded = raw
            .exclusions
            .iter()
            .filter(|row| row.language == doc.key.language && row.relative_path == doc.key.path)
            .cloned()
            .collect::<Vec<_>>();
        let capped_documents = raw
            .capped_documents
            .iter()
            .filter(|(language, path)| *language == doc.key.language && *path == doc.key.path)
            .cloned()
            .collect();
        let claims = document_claims(tx, capture.capture_id, &capture.artifact_hash, &doc.key)?;
        let captured_doc = captured_documents.get(&doc.key).copied();
        let witness = if reusable.contains(&doc.key) {
            let captured_doc =
                captured_doc.ok_or(WitnessFallback("reused_document_not_captured"))?;
            Some(document_witness(
                input,
                &original_key,
                &doc.key,
                captured_doc,
                doc,
                !excluded.is_empty(),
                !claims.is_empty(),
            )?)
        } else {
            None
        };
        let inventory = crate::scip_raw::ProofInventory {
            artifact_hashes: vec![capture.artifact_hash.clone()],
            source_manifest_hash: raw.source_manifest_hash.clone(),
            exclusions: excluded,
            capped_documents,
            claims,
        };
        let (reused, gaps) = match &witness {
            Some(witness) => {
                let original = witness.original();
                let Some(reused) = original_document_associations(
                    original,
                    &inventory,
                    &raw.producer_id,
                    &doc.key,
                ) else {
                    return witness_unavailable("original_associations_incomplete");
                };
                let gaps = original.coverage[0]
                    .reasons
                    .as_ref()
                    .map_or([0; 4], sem::CoverageReasons::gaps);
                current_targets(
                    input,
                    &validator,
                    required_original_targets(&reused),
                    &mut targets,
                    &mut absent,
                )?;
                (Some(reused), gaps)
            }
            None => (None, [0; 4]),
        };
        captured_header.revision.documents = captured_doc.cloned().into_iter().collect();
        destination_header.revision.documents = vec![doc.clone()];
        let Some(rejoined) = rejoin_document_with_witness(WitnessedRejoin {
            document: &doc.key,
            inventory: &inventory,
            captured_native: &captured_header,
            destination_native: &destination_header,
            producer,
            basis: &basis,
            reused,
            gaps,
            current_targets: &targets,
            destination: &facts,
            scope: &scope,
        })?
        else {
            return witness_unavailable("fused_reference_target_lost");
        };
        if !rejoined.dropped.is_empty()
            && shares_a_span(tx, capture.capture_id, &rejoined.dropped, &rejoined.fused)?
        {
            return witness_unavailable("fused_reference_target_lost");
        }
        let publication = rejoined.publication;
        let proofs = inventory.proof_lookup();
        let lookup = |proof: &str, kind: &str, digest: &str| {
            let claim = proofs.lookup(proof, kind, digest)?;
            Ok(sem::RawClaimDocument {
                language: claim.language.clone(),
                path: claim.path.clone(),
                artifact_hash: claim.artifact_hash.clone(),
            })
        };
        validator
            .document(&doc.key, &publication, witness.as_ref(), &targets, &lookup)
            .map_err(|error| witness_validation_failed(&error))?;
        write_import_coverage(tx, input.key, &publication)?;
        #[cfg(feature = "test-support")]
        if let Some(collected) = collected.as_deref_mut() {
            collected.coverage.extend(publication.coverage);
            collected.provenance.extend(publication.provenance);
            collected.symbols.extend(publication.symbols);
            collected
                .declaration_bindings
                .extend(publication.declaration_bindings);
            collected.references.extend(publication.references);
            collected.call_bindings.extend(publication.call_bindings);
            collected
                .type_relationships
                .extend(publication.type_relationships);
        }
    }
    Ok(())
}

fn witness_validation_failed(error: &anyhow::Error) -> anyhow::Error {
    tracing::debug!(%error, "witnessed semantic rejoin failed validation");
    WitnessFallback("witness_validation_failed").into()
}
