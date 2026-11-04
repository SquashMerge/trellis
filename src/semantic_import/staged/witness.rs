//! Bounded original-source reconstruction for the common settlement.
use super::*;
use crate::semantic_batch::staging::{WitnessFallback, original_rows_reservation};
use crate::store::native_first::witness::{CoverageInput, OriginalRow, OriginalSourceWitness};

pub(super) struct Prepared {
    pub facts: super::super::JoinedFacts,
    pub destination: super::super::JoinedFacts,
    pub reservation: Reservation,
}

fn fallback<T>(why: &'static str) -> Result<T> {
    Err(WitnessFallback(why).into())
}

pub(super) fn check_complete(join: &StagedJoin<'_>) -> Result<()> {
    if !exists(
        join.conn(),
        "SELECT 1 FROM witness_state WHERE singleton=1 AND internal_symbols_complete=1",
        &[],
    )? {
        return fallback("incomplete original internal symbol scope");
    }
    if exists(join.conn(), "SELECT 1 FROM claims LIMIT 1", &[])? {
        return fallback("canonical claims in witnessed settlement");
    }
    if exists(
        join.conn(),
        "SELECT 1 FROM source_witnesses WHERE complete<>1 LIMIT 1",
        &[],
    )? {
        return fallback("incomplete original source witness");
    }
    if exists(
        join.conn(),
        "SELECT 1 FROM nat_docs d WHERE d.rev=?1 AND EXISTS (SELECT 1 FROM sources s WHERE s.language=d.language AND s.path=d.path) AND NOT EXISTS \
         (SELECT 1 FROM sources s JOIN source_witnesses w ON w.source_ord=s.ord \
          WHERE s.language=d.language AND s.path=d.path AND w.complete=1) LIMIT 1",
        &[&join.destination.rev],
    )? {
        return fallback("current document lacks original source decision");
    }
    crate::semantic_batch::staging::check_proof_identity_closure(join.conn())?;
    Ok(())
}

fn header(
    join: &StagedJoin<'_>,
    doc: &native::DocumentKey,
) -> Result<(String, String, Reservation)> {
    let found: Option<(String,i64)> = join.conn().query_row(
        "SELECT w.decision,length(CAST(w.body AS BLOB)) FROM source_witnesses w JOIN sources s ON s.ord=w.source_ord \
         WHERE s.language=?1 AND s.path=?2 AND w.complete=1", params![doc.language,doc.path],
        |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((decision, bytes)) = found else {
        return fallback("missing complete source witness");
    };
    let reservation = join.ctx.live.reserve(
        u64::try_from(bytes)?
            .checked_mul(8)
            .and_then(|v| v.checked_add(1024))
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
    )?;
    let body = join.conn().query_row(
        "SELECT w.body FROM source_witnesses w JOIN sources s ON s.ord=w.source_ord \
         WHERE s.language=?1 AND s.path=?2",
        params![doc.language, doc.path],
        |r| r.get(0),
    )?;
    Ok((decision, body, reservation))
}

pub(super) fn document_flags(
    join: &StagedJoin<'_>,
    doc: &native::DocumentKey,
) -> Result<(DocFlags, Option<String>)> {
    if !exists(
        join.conn(),
        "SELECT 1 FROM sources WHERE language=?1 AND path=?2",
        &[&doc.language, &doc.path],
    )? {
        return Ok((DocFlags::default(), None));
    }
    let (decision, body, _reservation) = header(join, doc)?;
    let claims: i64 = join.conn().query_row(
        "SELECT COUNT(*) FROM proof_identities p JOIN sources s ON s.ord=p.source_ord \
        WHERE s.language=?1 AND s.path=?2 AND p.record_kind<>'symbolInformation'",
        params![doc.language, doc.path],
        |r| r.get(0),
    )?;
    let mut flags = DocFlags {
        claims: u64::try_from(claims)?,
        ..DocFlags::default()
    };
    if decision == "reused" {
        let witness: OriginalSourceWitness = serde_json::from_str(&body)?;
        if witness.document != *doc || witness.captured_version != witness.current_version {
            return fallback("source version witness differs");
        }
        let (_, current) = join
            .destination
            .doc_row(doc)?
            .context("missing witnessed current document")?;
        if current.content_hash != witness.source_hash
            || current.content_hash != witness.current_version.content_hash
        {
            return fallback("source hash witness differs");
        }
        if witness.reasons.as_ref().is_some_and(|r| r.omitted) {
            return fallback("original source omitted a dependent association");
        }
        flags.requested = witness.requested;
        flags.failed = witness.state == semantic::CoverageState::Failed;
        flags.eligible = witness.selected && !flags.failed;
        flags.capped = witness
            .capped
            .and_then(crate::scip_raw::DocumentCap::from_code);
        if let Some(r) = witness.reasons {
            flags.gaps = [
                r.macro_invocation,
                r.attribute_item,
                r.macro_definition,
                r.other,
            ];
            flags.unsupported = r.native_gaps() > 0;
            flags.unsupported_claims = r.native_gaps();
        }
        let excluded = if !witness.requested {
            Some("notRequested".to_owned())
        } else if let Some(exclusion) = &witness.exclusion {
            // The same raw exclusion FULL reads, so a failed exclusion keeps its diagnostic.
            Some(exclusion.clone())
        } else if witness.state == semantic::CoverageState::Omitted {
            Some("unsupported".to_owned())
        } else {
            None
        };
        Ok((flags, excluded))
    } else {
        let input: CoverageInput = serde_json::from_str(&body)?;
        if input.document != *doc {
            return fallback("factless source decision differs");
        }
        let (_, current) = join
            .destination
            .doc_row(doc)?
            .context("missing factless current document")?;
        if current.content_hash != input.current.content_hash {
            return fallback("factless source selected version differs");
        }
        flags.requested = input.requested;
        // A document the batch never requested stays notRequested even when its bytes changed,
        // as FULL reports it; only requested documents fail on unmeasurable captured anchors.
        flags.failed = input.requested
            && (decision == "changedBytes" || input.exclusion.as_deref() == Some("failed"));
        Ok((flags, input.exclusion))
    }
}

fn join_row(row: crate::store::native_first::witness::OriginalJoin) -> semantic::Join {
    semantic::Join {
        anchor: semantic::MeasuredAnchor {
            document: row.document,
            revision_id: row.revision_id,
            content_hash: row.content_hash,
            range: row.range,
            kind: row.kind,
        },
        status: row.status,
        candidate_ids: Vec::new(),
        diagnostic: row.diagnostic,
    }
}

pub(super) fn prepare(join: &StagedJoin<'_>, doc: &native::DocumentKey) -> Result<Prepared> {
    let (decision, body, _header_reservation) = header(join, doc)?;
    if decision != "reused" {
        return fallback("nonreusable source entered fact settlement");
    }
    let witness: OriginalSourceWitness = serde_json::from_str(&body)?;
    let source_ord: i64 = join.conn().query_row(
        "SELECT ord FROM sources WHERE language=?1 AND path=?2",
        params![doc.language, doc.path],
        |r| r.get(0),
    )?;
    // The typed event storage estimate admits decoded parents, children, indexing maps,
    // the final claim map and the destination copy before any body is materialized.
    let bytes = original_rows_reservation(join.conn(), source_ord)?;
    let reservation = join.ctx.live.reserve(
        bytes
            .checked_mul(3)
            .ok_or_else(|| ScipCaptureError::LimitExceeded("unit expansion".into()))?,
    )?;
    let mut symbol_order = Vec::new();
    let mut declaration_order = Vec::new();
    let mut call_order = Vec::new();
    let mut symbols = BTreeMap::<i64, semantic::Symbol>::new();
    let mut declarations = BTreeMap::<i64, semantic::DeclarationBinding>::new();
    let mut references = BTreeMap::<String, semantic::Reference>::new();
    let mut calls = BTreeMap::<i64, semantic::CallBinding>::new();
    let mut provenance = BTreeMap::new();
    let mut bases = BTreeMap::<i64, semantic::SemanticBasis>::new();
    let mut relationships = Vec::new();
    let mut coverage = None;
    let mut statement = join.conn().prepare_cached(
        "SELECT ordinal,body FROM original_associations WHERE source_ord=?1 ORDER BY ordinal",
    )?;
    let mut rows = statement.query([source_ord])?;
    let mut expected = 0i64;
    while let Some(row) = rows.next()? {
        join.ctx.check_cancel()?;
        join.ctx.charge(1)?;
        let ordinal: i64 = row.get(0)?;
        anyhow::ensure!(
            ordinal == expected,
            "original association event ordinal gap"
        );
        expected += 1;
        let body: String = row.get(1)?;
        let event: OriginalRow = serde_json::from_str(&body)?;
        match event {
            OriginalRow::Coverage(row) => {
                anyhow::ensure!(
                    coverage.replace(row).is_none(),
                    "duplicate original coverage"
                );
            }
            OriginalRow::CoverageRole {
                observed,
                ordinal,
                role,
            } => {
                let row = coverage.as_mut().context("coverage child before parent")?;
                let roles = if observed {
                    &mut row.observed_roles
                } else {
                    &mut row.supported_roles
                };
                append(roles, ordinal, role)?;
            }
            OriginalRow::Basis(row) => {
                let id = row.basis_id;
                let value = semantic::SemanticBasis {
                    producer_id: row.producer_id,
                    producer_version: row.producer_version,
                    producer_hash: row.producer_hash,
                    artifact_hash: row.artifact_hash,
                    language: row.language,
                    source_set_id: row.source_set_id,
                    revision_id: row.revision_id,
                    source_manifest_hash: row.source_manifest_hash,
                    toolchain_hash: row.toolchain_hash,
                    config_hash: row.config_hash,
                    dependency_hash: row.dependency_hash,
                    lookup_dependencies: Vec::new(),
                };
                anyhow::ensure!(
                    bases.insert(id, value).is_none(),
                    "duplicate original basis"
                );
            }
            OriginalRow::BasisLookupDependency {
                basis_id,
                ordinal,
                key,
            } => append(
                &mut bases
                    .get_mut(&basis_id)
                    .context("missing original basis")?
                    .lookup_dependencies,
                ordinal,
                key,
            )?,
            OriginalRow::Provenance(row) => {
                anyhow::ensure!(
                    provenance.insert(row.id.clone(), row).is_none(),
                    "duplicate original provenance"
                );
            }
            OriginalRow::Symbol(row) => {
                append(&mut symbol_order, row.ordinal, row.row_id)?;
                anyhow::ensure!(
                    symbols
                        .insert(
                            row.row_id,
                            semantic::Symbol {
                                key: row.key,
                                display_name: row.display_name,
                                declarations: Vec::new(),
                                provenance_id: row.provenance_id
                            }
                        )
                        .is_none(),
                    "duplicate original symbol"
                );
            }
            OriginalRow::SymbolTarget {
                row_id,
                ordinal,
                target,
            } => append(
                &mut symbols
                    .get_mut(&row_id)
                    .context("missing original symbol")?
                    .declarations,
                ordinal,
                target,
            )?,
            OriginalRow::DeclarationBinding(row) => {
                append(&mut declaration_order, row.ordinal, row.row_id)?;
                anyhow::ensure!(
                    declarations
                        .insert(
                            row.row_id,
                            semantic::DeclarationBinding {
                                syntax_id: row.syntax_id,
                                symbols: Vec::new(),
                                join: join_row(row.join),
                                provenance_id: row.provenance_id
                            }
                        )
                        .is_none(),
                    "duplicate original binding"
                );
            }
            OriginalRow::DeclarationSymbol {
                row_id,
                ordinal,
                key,
            } => append(
                &mut declarations
                    .get_mut(&row_id)
                    .context("missing original binding")?
                    .symbols,
                ordinal,
                key,
            )?,
            OriginalRow::DeclarationCandidate {
                row_id,
                ordinal,
                id,
            } => append(
                &mut declarations
                    .get_mut(&row_id)
                    .context("missing original binding")?
                    .join
                    .candidate_ids,
                ordinal,
                id,
            )?,
            OriginalRow::Reference(row) => {
                let id = row.id.clone();
                anyhow::ensure!(
                    references
                        .insert(
                            id,
                            semantic::Reference {
                                id: row.id,
                                owner_syntax_id: row.owner_syntax_id,
                                ordinal: usize::try_from(row.ordinal)?,
                                document: doc.clone(),
                                revision_id: join.captured_id().to_owned(),
                                range: row.range,
                                spelling: row.spelling,
                                lookup_key: row.lookup_key,
                                site: row.site,
                                roles: Vec::new(),
                                resolution: row.resolution,
                                declared_target: row.declared_target,
                                candidates: Vec::new(),
                                provenance_id: row.provenance_id
                            }
                        )
                        .is_none(),
                    "duplicate original reference"
                );
            }
            OriginalRow::ReferenceRole {
                reference_id,
                ordinal,
                role,
            } => append(
                &mut references
                    .get_mut(&reference_id)
                    .context("missing original reference")?
                    .roles,
                ordinal,
                role,
            )?,
            OriginalRow::ReferenceCandidate {
                reference_id,
                ordinal,
                target,
            } => append(
                &mut references
                    .get_mut(&reference_id)
                    .context("missing original reference")?
                    .candidates,
                ordinal,
                target,
            )?,
            OriginalRow::CallBinding(row) => {
                append(&mut call_order, row.ordinal, row.row_id)?;
                anyhow::ensure!(
                    calls
                        .insert(
                            row.row_id,
                            semantic::CallBinding {
                                call_id: row.call_id,
                                join: join_row(row.join),
                                resolution: row.resolution,
                                declared_target: row.declared_target,
                                candidates: Vec::new(),
                                dispatch: row.dispatch,
                                possible_dispatch: Vec::new(),
                                possible_dispatch_complete: row.possible_dispatch_complete,
                                stale_target: row.stale_target,
                                provenance_id: row.provenance_id
                            }
                        )
                        .is_none(),
                    "duplicate original call"
                );
            }
            OriginalRow::CallJoinCandidate {
                row_id,
                ordinal,
                id,
            } => append(
                &mut calls
                    .get_mut(&row_id)
                    .context("missing original call")?
                    .join
                    .candidate_ids,
                ordinal,
                id,
            )?,
            OriginalRow::CallCandidate {
                row_id,
                ordinal,
                target,
            } => append(
                &mut calls
                    .get_mut(&row_id)
                    .context("missing original call")?
                    .candidates,
                ordinal,
                target,
            )?,
            OriginalRow::CallDispatch {
                row_id,
                ordinal,
                target,
            } => append(
                &mut calls
                    .get_mut(&row_id)
                    .context("missing original call")?
                    .possible_dispatch,
                ordinal,
                target,
            )?,
            OriginalRow::TypeRelationship { ordinal, row } => {
                append(&mut relationships, ordinal, row)?
            }
        }
    }
    let coverage = coverage.ok_or(WitnessFallback("original source has no coverage"))?;
    if coverage.document_path != doc.path
        || coverage.source_set_id != doc.source_set_id
        || coverage.producer_id != join.producer.id
        || coverage.revision_id != join.captured_id()
        || witness.captured_version != witness.current_version
    {
        return fallback("original source tuple identity differs");
    }
    let mut claimed = BTreeSet::new();
    let mut raw_for = |id: &str| -> Result<String> {
        let p = provenance
            .get(id)
            .context("original fact has no provenance")?;
        let basis = bases
            .get(&p.basis_id)
            .context("original provenance has no basis")?;
        if p.content_hash != witness.source_hash
            || basis.revision_id != join.captured_id()
            || basis.producer_id != join.producer.id
            || basis.source_manifest_hash != join.descriptor.source_manifest_hash
        {
            return fallback("original proof authority differs");
        }
        let derived = &p.derived_from;
        let kind = serde_json::to_value(derived.record_kind)?;
        let kind = kind.as_str().context("invalid original record kind")?;
        let (raw, _) =
            join.filtered
                .proof_identity(&derived.provenance_id, kind, &derived.fact_digest)?;
        anyhow::ensure!(
            raw.language == doc.language
                && raw.path == doc.path
                && raw.artifact_hash == basis.artifact_hash,
            "original proof document differs"
        );
        let capture: Option<i64> = join.conn().query_row(
            "SELECT capture_id FROM proof_identities WHERE proof_id=?1",
            [&derived.provenance_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            capture == Some(p.capture_id),
            "original proof capture differs"
        );
        claimed.insert(id.to_owned());
        Ok(derived.provenance_id.clone())
    };
    let mut facts = super::super::JoinedFacts::default();
    for id in symbol_order {
        let mut row = symbols.remove(&id).context("missing ordered symbol")?;
        row.provenance_id = raw_for(&row.provenance_id)?;
        facts.symbols.push(row);
    }
    for id in declaration_order {
        let mut row = declarations
            .remove(&id)
            .context("missing ordered declaration")?;
        row.provenance_id = raw_for(&row.provenance_id)?;
        facts.declaration_bindings.push(row);
    }
    for mut row in references.into_values() {
        row.provenance_id = raw_for(&row.provenance_id)?;
        if facts
            .reference_claims
            .insert(row.provenance_id.clone(), row.clone())
            .is_some()
        {
            return fallback("multiple original references for one raw proof");
        }
        facts.references.push(row);
    }
    for id in call_order {
        let mut row = calls.remove(&id).context("missing ordered call")?;
        row.provenance_id = raw_for(&row.provenance_id)?;
        if !facts.reference_claims.contains_key(&row.provenance_id) {
            let anchor = &row.join.anchor;
            let mut matches = facts.references.iter().filter(|reference| {
                reference.roles.contains(&semantic::Role::Call)
                    && (reference.range == anchor.range
                        || (anchor.kind == semantic::JoinKind::Invocation
                            && anchor.range.start <= reference.range.start
                            && reference.range.end <= anchor.range.end))
            });
            let Some(reference) = matches.next() else {
                return fallback("missing original fused call reference");
            };
            if matches.next().is_some() {
                return fallback("ambiguous original fused call reference");
            }
            let mut reference = reference.clone();
            facts.fused_calls.insert(reference.provenance_id.clone());
            facts.fused_calls.insert(row.provenance_id.clone());
            reference.provenance_id = row.provenance_id.clone();
            facts
                .reference_claims
                .insert(row.provenance_id.clone(), reference);
        }
        facts.call_bindings.push(row);
    }
    for mut row in relationships {
        row.provenance_id = raw_for(&row.provenance_id)?;
        facts.type_relationships.push(row);
    }
    for (id, p) in &provenance {
        if claimed.contains(id) {
            continue;
        }
        if p.evidence_kind != semantic::EvidenceKind::SemanticReference {
            return fallback("missing original proof association");
        }
        let basis = bases
            .get(&p.basis_id)
            .context("original fused proof has no basis")?;
        let derived = &p.derived_from;
        let kind = serde_json::to_value(derived.record_kind)?;
        let (raw, _) = join.filtered.proof_identity(
            &derived.provenance_id,
            kind.as_str().context("invalid original kind")?,
            &derived.fact_digest,
        )?;
        if p.content_hash != witness.source_hash
            || basis.revision_id != join.captured_id()
            || basis.producer_id != join.producer.id
            || raw.language != doc.language
            || raw.path != doc.path
            || raw.artifact_hash != basis.artifact_hash
        {
            return fallback("original fused proof authority differs");
        }
        let capture: Option<i64> = join.conn().query_row(
            "SELECT capture_id FROM proof_identities WHERE proof_id=?1",
            [&derived.provenance_id],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            capture == Some(p.capture_id),
            "original fused proof capture differs"
        );
        facts.fused_proofs.insert(derived.provenance_id.clone());
    }
    if coverage
        .reasons
        .as_ref()
        .is_some_and(|r| r.captured_diagnostics)
    {
        facts.diagnostics.insert(doc.clone(), BTreeSet::new());
    }
    let mut destination = facts.clone();
    for row in &mut destination.declaration_bindings {
        row.join.anchor.revision_id = join.destination_id().to_owned();
    }
    for row in &mut destination.references {
        row.revision_id = join.destination_id().to_owned();
    }
    for row in destination.reference_claims.values_mut() {
        row.revision_id = join.destination_id().to_owned();
    }
    for row in &mut destination.call_bindings {
        row.join.anchor.revision_id = join.destination_id().to_owned();
        // A reused row is no destination classification of a target outside the reused
        // documents: the rejoin takes the captured one under its own rule, as FULL does (#557).
        if let Some(semantic::Target::Internal { document, .. }) = &row.declared_target
            && !reused_source(join, document)?
        {
            row.declared_target = None;
        }
    }
    Ok(Prepared {
        facts,
        destination,
        reservation,
    })
}

/// Whether `doc`'s source association is reused unchanged from the original import.
fn reused_source(join: &StagedJoin<'_>, doc: &native::DocumentKey) -> Result<bool> {
    exists(
        join.conn(),
        "SELECT 1 FROM source_witnesses w JOIN sources s ON s.ord=w.source_ord \
         WHERE s.language=?1 AND s.path=?2 AND w.decision='reused' AND w.complete=1",
        &[&doc.language, &doc.path],
    )
}

fn append<T>(values: &mut Vec<T>, ordinal: i64, value: T) -> Result<()> {
    anyhow::ensure!(
        usize::try_from(ordinal)? == values.len(),
        "original child ordinal gap"
    );
    values.push(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn original_children_require_contiguous_ordinals() {
        let mut values = Vec::new();
        append(&mut values, 0, "first").unwrap();
        assert!(append(&mut values, 2, "gap").is_err());
        assert!(append(&mut values, 0, "duplicate").is_err());
        append(&mut values, 1, "second").unwrap();
        assert_eq!(values, ["first", "second"]);
    }
}
