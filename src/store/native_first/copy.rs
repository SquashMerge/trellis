//! Owner ruling 2026-11-04 (#669 H1): for an unchanged document, S copies the previously
//! published semantic rows to the new revision instead of re-staging and re-settling them.
//!
//! A document is copied when the older-basis projection already serves it (its projection
//! document is not pending: same document version and context, no dependency on changed
//! inputs) and the witness admitted it as reused. Its rows are the basis pin's tuple, which
//! S or an import validated and published; this trusts them as reads do (#261). The copy
//! re-keys exactly what the witnessed settlement derives from the destination: the native
//! revision named by coverage, references, join anchors and kept internal targets, the
//! rejoined provenance IDs and the provenance freshness. Corruption read while copying is
//! the read path's typed integrity failure, so it schedules a re-index as reads do.
use super::*;
use crate::native_evidence::DocumentKey;
use crate::semantic_evidence as sem;

/// What a copied row takes from the destination native revision.
pub(crate) struct CopyDestination {
    native_revision_id: String,
    toolchain_hash: String,
    config_hash: String,
    dependency_hash: String,
    manifest_hash: String,
}

impl CopyDestination {
    /// The pending target's native revision and source manifest, as the join's settlement
    /// reads them (`SourceManifestHasher` over the canonical inventory).
    pub(crate) fn read(
        store: &Store,
        db: &Connection,
        target: &PendingSemanticTarget,
    ) -> Result<Self> {
        let key = semantic_pin_key(target.pin);
        let (native_revision_id, source_set_id, toolchain_hash, config_hash, dependency_hash): (
            String,
            String,
            String,
            String,
            String,
        ) = db
            .prepare_cached(
                "SELECT native_revision_id,source_set_id,toolchain_hash,config_hash,dependency_hash FROM native_revisions WHERE id=?1",
            )?
            .query_row([&key], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
        selected_ensure!(
            native_revision_id == target.native_revision_id,
            "incompatible_index: copy destination differs from the pending target"
        );
        let read = ReadRevision::new(target.pin, key);
        let reader = NativeReader {
            store,
            db,
            selected: &read,
        };
        let mut manifest =
            crate::semantic_batch::descriptor::SourceManifestHasher::new(&source_set_id);
        let mut cursor = CanonicalInventoryCursor::default();
        loop {
            let page = reader.canonical_inventory_page(&mut cursor, 256)?;
            if page.is_empty() {
                break;
            }
            for document in &page {
                manifest.push(
                    &document.key.language,
                    &document.key.path,
                    &document.content_hash,
                );
            }
        }
        Ok(Self {
            native_revision_id,
            toolchain_hash,
            config_hash,
            dependency_hash,
            manifest_hash: manifest.finish(),
        })
    }
}

/// The basis pin key and tuple of `document` when S copies it for `producer`: the
/// older-basis projection serves it at the pending target (not pending), the basis pin
/// published a tuple for it, and that tuple omitted no association and is no unavailable S's
/// failure.
pub(crate) fn copy_source(
    db: &Connection,
    target: &PendingSemanticTarget,
    producer: &str,
    document: &DocumentKey,
) -> Result<Option<(String, i64)>> {
    // A basis tuple that omitted rows for an absent target is not copied: the target can
    // return without the document changing, and only the raw claims can tell (the witness's
    // own rule for omitted original associations). Nor is a tuple an unavailable S published
    // (#826): it is no producer evidence, and the retained selection can still be re-joined.
    let row: Option<(bool, String, Option<i64>)> = db
        .prepare_cached(
            "SELECT d.pending OR EXISTS(SELECT 1 FROM semantic_coverage_reasons r WHERE r.tuple_id=c.tuple_id AND r.omitted=1) \
             OR COALESCE(c.state='failed' AND substr(c.diagnostic,1,length(?5)+1)=?5||':',0),\
             o.basis_pin,c.tuple_id FROM semantic_projection_documents d \
             JOIN semantic_projection_origins o ON o.revision_id=d.revision_id AND o.producer_id=d.producer_id \
             LEFT JOIN semantic_coverage c ON c.revision_id=o.basis_pin AND c.producer_id=o.producer_id \
             AND c.language=d.language AND c.path=d.path \
             WHERE d.revision_id=?1 AND d.producer_id=?2 AND d.language=?3 AND d.path=?4",
        )?
        .query_row(
            params![
                semantic_pin_key(target.pin),
                producer,
                document.language,
                document.path,
                UNAVAILABLE_DIAGNOSTIC
            ],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    Ok(match row {
        Some((false, basis, Some(tuple))) => Some((basis, tuple)),
        _ => None,
    })
}

/// What decides, per document, whether S copies it for one producer: the pending target and
/// the producer's captured pin. Admission (which then stages none of the document's proof
/// identities) and witness preparation (which stages it as copied) ask the same question.
#[derive(Clone)]
pub(crate) struct CopyFilter {
    pub(crate) target: PendingSemanticTarget,
    pub(crate) captured_pin: IndexPin,
    pub(crate) producer: String,
}

impl CopyFilter {
    /// Whether S copies `document`: [`copy_source`] names its basis tuple, and its captured
    /// and current versions are the same (the witness's reuse condition), so the copied rows
    /// are exactly what re-settling it would emit.
    pub(crate) fn copies(&self, db: &Connection, document: &DocumentKey) -> Result<bool> {
        if copy_source(db, &self.target, &self.producer, document)?.is_none() {
            return Ok(false);
        }
        let current = witness::selected_version(db, self.target.pin, document)?;
        Ok(current.is_some()
            && witness::selected_version(db, self.captured_pin, document)? == current)
    }
}

/// The publication S writes for a copied document: the basis tuple re-keyed to the
/// destination, exactly as the witnessed settlement would emit it.
pub(crate) fn copied_document(
    db: &Connection,
    basis: &str,
    tuple: i64,
    producer: &sem::Producer,
    destination: &CopyDestination,
) -> Result<sem::SemanticPublication> {
    let mut snapshot = read_semantic_tuple(db, basis, Some(tuple), None, true)?;
    let [coverage] = snapshot.coverage.as_mut_slice() else {
        return Err(
            SelectedIntegrity("incompatible_index: copied tuple coverage missing".into()).into(),
        );
    };
    selected_ensure!(
        coverage.producer_id == producer.id,
        "incompatible_index: copied tuple producer differs"
    );
    coverage.reasons = read_coverage_reasons(db, tuple)?;
    let old = std::mem::replace(
        &mut coverage.revision_id,
        destination.native_revision_id.clone(),
    );
    let new = &destination.native_revision_id;
    let revision = |id: &mut String| {
        if *id == old {
            *id = new.clone();
        }
    };
    let target = |target: &mut sem::Target| {
        if let sem::Target::Internal { revision_id, .. } = target
            && *revision_id == old
        {
            *revision_id = new.clone();
        }
    };
    let mut ids = BTreeMap::new();
    for provenance in &mut snapshot.provenance {
        let derived = provenance.derived_from.as_ref().context(SelectedIntegrity(
            "incompatible_index: copied provenance is not derived".into(),
        ))?;
        let basis = provenance.basis.as_ref().context(SelectedIntegrity(
            "incompatible_index: copied provenance has no basis".into(),
        ))?;
        let id = crate::semantic_import::rejoin_provenance_id(
            &derived.provenance_id,
            new,
            provenance.evidence_kind,
        );
        // The settlement's freshness rule for an unchanged document (its captured and
        // destination bytes agree, which the witness checked before choosing the copy).
        provenance.freshness = if basis.revision_id != *new
            || basis.toolchain_hash != destination.toolchain_hash
            || basis.source_manifest_hash != destination.manifest_hash
            || basis.config_hash != destination.config_hash
            || basis.dependency_hash != destination.dependency_hash
            || basis.producer_version != producer.version
            || basis.producer_hash != producer.executable_hash
            || !producer.languages.contains(&basis.language)
        {
            sem::Freshness::PossiblyStale
        } else {
            sem::Freshness::Fresh
        };
        revision(&mut provenance.revision_id);
        ids.insert(std::mem::replace(&mut provenance.id, id.clone()), id);
    }
    let rekey = |id: &mut String| -> Result<()> {
        *id = ids
            .get(id.as_str())
            .context(SelectedIntegrity(
                "incompatible_index: copied fact without provenance".into(),
            ))?
            .clone();
        Ok(())
    };
    for symbol in &mut snapshot.symbols {
        symbol.declarations.iter_mut().for_each(target);
        rekey(&mut symbol.provenance_id)?;
    }
    for binding in &mut snapshot.declaration_bindings {
        revision(&mut binding.join.anchor.revision_id);
        rekey(&mut binding.provenance_id)?;
    }
    for reference in &mut snapshot.references {
        revision(&mut reference.revision_id);
        reference.declared_target.iter_mut().for_each(target);
        reference.candidates.iter_mut().for_each(target);
        rekey(&mut reference.provenance_id)?;
    }
    for call in &mut snapshot.call_bindings {
        revision(&mut call.join.anchor.revision_id);
        call.declared_target.iter_mut().for_each(target);
        call.candidates.iter_mut().for_each(target);
        call.possible_dispatch.iter_mut().for_each(target);
        rekey(&mut call.provenance_id)?;
    }
    for relationship in &mut snapshot.type_relationships {
        target(&mut relationship.source);
        target(&mut relationship.target);
        rekey(&mut relationship.provenance_id)?;
    }
    Ok(sem::SemanticPublication {
        producers: vec![producer.clone()],
        coverage: snapshot.coverage,
        provenance: snapshot.provenance,
        symbols: snapshot.symbols,
        declaration_bindings: snapshot.declaration_bindings,
        references: snapshot.references,
        call_bindings: snapshot.call_bindings,
        type_relationships: snapshot.type_relationships,
    })
}

/// The documents an admitted witnessed stage marked as copied, in canonical key order.
pub(crate) fn copied_documents(
    stage: &Connection,
    source_set_id: &str,
) -> Result<Vec<DocumentKey>> {
    Ok(stage
        .prepare(
            "SELECT s.language,s.path FROM source_witnesses w JOIN sources s ON s.ord=w.source_ord \
             WHERE w.decision='copied' ORDER BY s.language,s.path",
        )?
        .query_map([], |r| {
            Ok(DocumentKey {
                source_set_id: source_set_id.to_owned(),
                language: r.get(0)?,
                path: r.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?)
}
