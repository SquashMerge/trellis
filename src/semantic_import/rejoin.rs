//! The rejoin chain: the in-memory oracle for the staged join, and destination mutations.
#[cfg(feature = "test-support")]
use super::work::count_publication_work;
use super::{
    coordinates::{CoordinateMaps, InventoryIndex},
    join::{JoinedFacts, join_into_facts},
    join_store::{BasisFor, IndexedJoinStore, NativeFacts, ProofIdentities, PublicationSink},
    measured::MeasuredFacts,
    memory::MemoryJoinStore,
    rejoin_provenance_id, rejoined_dispatch,
    report::LanguageReasons,
};
use crate::{
    native_evidence as native, native_ids, scip_raw::CapturedScipInventory,
    semantic_evidence as semantic,
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

/// Publication-time rejoin uses only retained, authenticated source and measured snapshots.
/// The Store validates the returned publication and its raw links before its paired-pin CAS.
#[derive(Debug)]
pub struct RejoinResult {
    pub publication: semantic::SemanticPublication,
    pub report: serde_json::Value,
}

#[allow(clippy::too_many_arguments)]
pub fn rejoin_inventory(
    inventory: &CapturedScipInventory,
    captured_native: &native::Artifact,
    captured_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    destination_native: &native::Artifact,
    destination_sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    admitted_encoding: u8,
) -> Result<RejoinResult> {
    // The destination facts are measured first, as before: their error precedes the rest.
    let r2 = MeasuredFacts::new(destination_native, destination_sources)?;
    ensure!(
        inventory.source_manifest_hash == manifest_hash(&captured_native.revision)
            && captured_native.revision.source_set_id == destination_native.revision.source_set_id,
        "captured source manifest/source set mismatch"
    );
    let r1 = MeasuredFacts::new(captured_native, captured_sources)?;
    let raw_index = InventoryIndex::new(inventory)?;
    let coordinates = CoordinateMaps::new(
        inventory,
        captured_native,
        captured_sources,
        admitted_encoding,
    )?;
    let captured_store = MemoryJoinStore::new(&raw_index, &coordinates, &r1, captured_sources);
    let destination_store =
        MemoryJoinStore::new(&raw_index, &coordinates, &r2, destination_sources);
    rejoin_with_stores(
        inventory,
        captured_native,
        destination_native,
        producer,
        basis,
        admitted_encoding,
        &captured_store,
        &destination_store,
    )
}

#[allow(clippy::too_many_arguments)]
fn rejoin_with_stores<S: IndexedJoinStore + NativeFacts>(
    inventory: &CapturedScipInventory,
    captured_native: &native::Artifact,
    destination_native: &native::Artifact,
    producer: semantic::Producer,
    basis: &BTreeMap<native::DocumentKey, semantic::SemanticBasis>,
    admitted_encoding: u8,
    captured_store: &S,
    destination_store: &S,
) -> Result<RejoinResult> {
    let captured = join_into_facts(admitted_encoding, captured_store)?;
    let captured_declaration_proofs: BTreeSet<_> = captured
        .declaration_bindings
        .iter()
        .filter(|binding| binding.syntax_id.is_some())
        .map(|binding| binding.provenance_id.as_str())
        .collect();
    let captured_calls: BTreeMap<_, _> = captured
        .call_bindings
        .iter()
        .filter(|binding| binding.call_id.is_some())
        .map(|binding| (binding.provenance_id.as_str(), binding))
        .collect();
    let captured_association_proofs: BTreeSet<_> = captured_declaration_proofs
        .iter()
        .copied()
        .chain(captured.reference_claims.keys().map(String::as_str))
        .chain(captured_calls.keys().copied())
        .chain(
            captured
                .type_relationships
                .iter()
                .map(|relationship| relationship.provenance_id.as_str()),
        )
        .collect();
    let mut eligible = BTreeSet::new();
    let mut unsupported_claims = BTreeSet::new();
    // Each document's unsupported claims by the kind of native gap that hid them (#520).
    let mut unsupported_documents: BTreeMap<native::DocumentKey, [u64; 4]> = BTreeMap::new();
    let mut failed = BTreeSet::new();
    let mut requested = BTreeSet::new();
    let unchanged_documents: BTreeSet<_> = captured_store
        .source_documents()
        .filter(|document| {
            destination_store.source_bytes(document) == captured_store.source_bytes(document)
        })
        .cloned()
        .collect();
    let exclusions: BTreeMap<_, _> = inventory
        .exclusions
        .iter()
        .map(|exclusion| {
            (
                (
                    exclusion.language.as_str(),
                    exclusion.relative_path.as_str(),
                ),
                exclusion.reason.as_str(),
            )
        })
        .collect();
    for document in &captured_native.revision.documents {
        let language: semantic::Language =
            serde_json::from_value(serde_json::json!(document.key.language))?;
        if producer.languages.contains(&language) {
            let reason =
                exclusions.get(&(document.key.language.as_str(), document.key.path.as_str()));
            if reason != Some(&"notRequested") {
                requested.insert(document.key.clone());
            }
            if reason == Some(&"failed")
                || (reason != Some(&"notRequested") && !unchanged_documents.contains(&document.key))
            {
                failed.insert(document.key.clone());
            }
        }
    }
    for claim in captured_store.claims() {
        let claim = claim?;
        captured_store.charge(1)?;
        captured_store.lookup_proof(
            &claim.captured_proof_id,
            &claim.envelope.record_kind,
            &claim.fact_digest,
        )?;
        let doc = native::DocumentKey {
            source_set_id: captured_native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        requested.insert(doc.clone());
        captured_store
            .document(&doc)
            .context("captured claim outside source manifest")?;
        // MeasuredFacts::new authenticated each document once, not once per claim.
        captured_store
            .source_bytes(&doc)
            .context("missing captured claim source")?;
        if !unchanged_documents.contains(&doc) {
            failed.insert(doc);
            continue;
        }
        if claim.envelope.record_kind == "occurrence" {
            let range = captured_store.occurrence_range(&claim, admitted_encoding)?;
            if range.start == range.end {
                eligible.insert(doc);
                continue;
            }
            #[cfg(feature = "test-support")]
            count_publication_work(|work| work.original_role_parses += 1);
            let roles = claim.envelope.raw["symbolRoles"]
                .as_u64()
                .context("invalid raw roles")?;
            // Rejoin only the families that actually authored a captured source fact.
            // A tempting callee at the same bytes cannot replace a missing reference,
            // nor can an enclosing range alone turn a plain reference into a call.
            let captured_declaration =
                captured_declaration_proofs.contains(claim.captured_proof_id.as_str());
            let captured_reference = captured
                .reference_claims
                .contains_key(&claim.captured_proof_id);
            let required = if captured_declaration || (!captured_reference && roles & 1 != 0) {
                semantic::JoinKind::DeclarationName
            } else {
                semantic::JoinKind::Reference
            };
            let mut measured_operation = destination_store.operation()?;
            let first = captured_store.join(&doc, range.clone(), required)?;
            let second = destination_store.join(&doc, range, required)?;
            // Only claims without a captured semantic association may keep their
            // unavailable outcome without failing the caller tuple.
            let authored_association =
                captured_association_proofs.contains(claim.captured_proof_id.as_str());
            if !authored_association
                && first.status == semantic::JoinStatus::Unsupported
                && second.status == semantic::JoinStatus::Unsupported
            {
                unsupported_claims.insert(claim.captured_proof_id.clone());
                unsupported_documents.entry(doc.clone()).or_default()
                    [semantic::CoverageReasons::gap_kind(first.diagnostic.as_deref())] += 1;
                eligible.insert(doc);
                continue;
            }
            if !authored_association
                && first.status == semantic::JoinStatus::Unmatched
                && second.status == semantic::JoinStatus::Unmatched
            {
                eligible.insert(doc);
                continue;
            }
            let mut exact = first.status == semantic::JoinStatus::Exact
                && second.status == semantic::JoinStatus::Exact;
            if captured_reference && required != semantic::JoinKind::Reference {
                let first_reference = captured_store.join(
                    &doc,
                    first.anchor.range.clone(),
                    semantic::JoinKind::Reference,
                )?;
                let second_reference = destination_store.join(
                    &doc,
                    first.anchor.range.clone(),
                    semantic::JoinKind::Reference,
                )?;
                exact &= first_reference.status == semantic::JoinStatus::Exact
                    && second_reference.status == semantic::JoinStatus::Exact;
            }
            let captured_call = captured_calls
                .get(claim.captured_proof_id.as_str())
                .copied();
            if let Some(binding) = captured_call {
                let call = destination_store.join(
                    &doc,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                )?;
                let first_call = captured_store.join(
                    &doc,
                    binding.join.anchor.range.clone(),
                    binding.join.anchor.kind,
                )?;
                exact &= call.status == semantic::JoinStatus::Exact
                    && first_call.status == semantic::JoinStatus::Exact
                    && first_call.candidate_ids == binding.join.candidate_ids;
                // The selected reference and invocation must belong to that same
                // measured call, not merely be independent exact spans.
                let mut same_call = false;
                if let Some(id) = second.candidate_ids.first() {
                    let calls = match call.candidate_ids.first() {
                        Some(call_id) => destination_store.calls_with_id(
                            &doc,
                            call_id,
                            &mut measured_operation,
                        )?,
                        None => vec![],
                    };
                    same_call = destination_store
                        .references_with_id(&doc, id, &mut measured_operation)?
                        .iter()
                        .any(|reference| {
                            &reference.id == id
                                && calls.iter().any(|candidate| {
                                    call.candidate_ids == vec![candidate.id.clone()]
                                        && candidate.owner_syntax_id == reference.owner_syntax_id
                                        && candidate.range.start <= reference.range.start
                                        && reference.range.end <= candidate.range.end
                                        && candidate
                                            .callee_range
                                            .as_ref()
                                            .is_none_or(|callee| callee == &reference.range)
                                })
                        });
                }
                exact &= same_call;
            }
            if exact {
                eligible.insert(doc);
            } else {
                failed.insert(doc);
            }
        } else {
            eligible.insert(doc);
        }
    }
    eligible.retain(|doc| !failed.contains(doc));
    let mut filtered = inventory.clone();
    filtered.claims.retain(|claim| {
        eligible
            .iter()
            .any(|doc| doc.language == claim.language && doc.path == claim.envelope.relative_path)
    });
    let filtered_store = destination_store.restrict(&filtered)?;
    let destination = join_into_facts(admitted_encoding, &filtered_store)?;
    let metadata = inventory.proof_identities();
    let filtered_metadata = filtered.proof_identities();
    let mut publication = empty_publication(&producer);
    let report = finish_rejoin(
        &metadata,
        captured_native,
        destination_native,
        producer,
        basis,
        captured_store,
        destination_store,
        captured_store,
        &filtered_store,
        &filtered_metadata,
        &exclusions,
        captured,
        destination,
        requested,
        failed,
        unsupported_claims,
        unsupported_documents,
        &mut publication,
    )?;
    Ok(RejoinResult {
        publication,
        report,
    })
}

/// Test-support edits to the destination's measured facts, applied only when the captured and
/// destination revisions differ. Both join backends take them as a value; the in-memory
/// destination applies them to the whole artifact, the staged one to each document it reads.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DestinationMutations {
    pub duplicate_b_target: bool,
    pub duplicate_a_reference: bool,
    pub missing_a_reference: bool,
    /// B's `target` declaration gets another syntax ID: a destination target that is not a
    /// captured one.
    pub renamed_b_target: bool,
}

impl DestinationMutations {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Reads the `TRELLIS_TEST_SEMANTIC_*` hook variables.
    #[cfg(feature = "test-support")]
    pub fn from_env() -> Self {
        Self {
            duplicate_b_target: std::env::var_os("TRELLIS_TEST_SEMANTIC_DUPLICATE_B_TARGET")
                .is_some(),
            duplicate_a_reference: std::env::var_os("TRELLIS_TEST_SEMANTIC_DUPLICATE_A_REFERENCE")
                .is_some(),
            missing_a_reference: std::env::var_os("TRELLIS_TEST_SEMANTIC_MISSING_A_REFERENCE")
                .is_some(),
            renamed_b_target: std::env::var_os("TRELLIS_TEST_SEMANTIC_RENAMED_B_TARGET").is_some(),
        }
    }

    /// Edits `declarations` and `references`, which may be the whole artifact's or one
    /// document's.
    pub fn apply(
        &self,
        declarations: &mut Vec<native::Declaration>,
        references: &mut Vec<native::Reference>,
    ) {
        if self.duplicate_b_target
            && let Some(declaration) = declarations
                .iter()
                .find(|d| d.document.path == "src/B.js" && d.name.as_deref() == Some("target"))
                .cloned()
        {
            declarations.push(declaration);
        }
        if self.duplicate_a_reference
            && let Some(reference) = references
                .iter()
                .find(|r| r.document.path == "src/A.js" && r.spelling == "target")
                .cloned()
        {
            let mut competing = reference;
            competing.id = format!("{}-candidate-only", competing.id);
            references.push(competing);
        }
        if self.renamed_b_target
            && let Some(declaration) = declarations
                .iter_mut()
                .find(|d| d.document.path == "src/B.js" && d.name.as_deref() == Some("target"))
        {
            // Another canonical syntax ID: flip the last hex digit.
            let last = declaration.syntax_id.pop().unwrap_or('0');
            declaration
                .syntax_id
                .push(if last == '0' { '1' } else { '0' });
        }
        if self.missing_a_reference {
            references.retain(|r| !(r.document.path == "src/A.js" && r.spelling == "target"));
        }
    }
}

pub(crate) fn empty_publication(producer: &semantic::Producer) -> semantic::SemanticPublication {
    semantic::SemanticPublication {
        producers: vec![producer.clone()],
        coverage: Vec::new(),
        provenance: Vec::new(),
        symbols: Vec::new(),
        declaration_bindings: Vec::new(),
        references: Vec::new(),
        call_bindings: Vec::new(),
        type_relationships: Vec::new(),
    }
}

#[allow(clippy::too_many_arguments)]
fn finish_rejoin<F: NativeFacts, P: ProofIdentities, Q: ProofIdentities, B: BasisFor>(
    inventory: &crate::scip_raw::ProofInventory,
    captured_native: &native::Artifact,
    destination_native: &native::Artifact,
    producer: semantic::Producer,
    basis: &B,
    r1: &F,
    r2: &F,
    raw_index: &P,
    filtered_index: &Q,
    filtered: &crate::scip_raw::ProofInventory,
    exclusions: &BTreeMap<(&str, &str), &str>,
    captured: JoinedFacts,
    destination: JoinedFacts,
    requested: BTreeSet<native::DocumentKey>,
    failed: BTreeSet<native::DocumentKey>,
    unsupported_claims: BTreeSet<String>,
    unsupported_documents: BTreeMap<native::DocumentKey, [u64; 4]>,
    sink: &mut impl PublicationSink,
) -> Result<serde_json::Value> {
    let mut captured_targets: BTreeMap<(native::DocumentKey, String), Vec<semantic::Target>> =
        BTreeMap::new();
    for binding in &captured.declaration_bindings {
        if let Some(syntax_id) = &binding.syntax_id {
            for symbol in &binding.symbols {
                captured_targets
                    .entry((binding.join.anchor.document.clone(), symbol.symbol.clone()))
                    .or_default()
                    .push(semantic::Target::Internal {
                        syntax_id: syntax_id.clone(),
                        document: binding.join.anchor.document.clone(),
                        revision_id: captured_native.revision.id.clone(),
                    });
            }
        }
    }
    for targets in captured_targets.values_mut() {
        targets.sort_by_key(|target| {
            native_ids::canonical(&serde_json::to_value(target).expect("serializable target"))
        });
        targets.dedup();
    }
    let captured_declarations: BTreeMap<_, _> = captured
        .declaration_bindings
        .iter()
        .filter_map(|b| {
            b.syntax_id
                .as_ref()
                .map(|id| (b.provenance_id.clone(), id.clone()))
        })
        .collect();
    let mut omitted = BTreeSet::new();
    let internal_symbols: BTreeSet<_> = captured_targets
        .iter()
        .filter(|(_, targets)| !targets.is_empty())
        .map(|((_, symbol), _)| symbol.clone())
        .collect();
    let retarget = |target: &mut semantic::Target| -> bool {
        match target {
            semantic::Target::External { symbol } if internal_symbols.contains(&symbol.symbol) => {
                false
            }
            semantic::Target::Internal {
                syntax_id,
                document,
                revision_id,
            } => {
                let original = r1.declarations_by_id(syntax_id).iter().any(|d| {
                    d.syntax_id == *syntax_id
                        && d.document == *document
                        && semantic::retargetable(d)
                });
                if !original {
                    return false;
                }
                let candidates: Vec<_> = r2
                    .declarations_by_id(syntax_id)
                    .iter()
                    .copied()
                    .filter(|d| {
                        d.syntax_id == *syntax_id
                            && d.document == *document
                            && semantic::retargetable(d)
                    })
                    .collect();
                if candidates.len() != 1 || r2.source_bytes(document).is_none() {
                    return false;
                }
                let declaration = candidates[0];
                // A root module has no name to recheck (#1235).
                if let Some(name) = declaration.name_range.as_ref()
                    && r2
                        .source(document)
                        .and_then(|s| s.get(name.start..name.end))
                        != declaration.name.as_deref()
                {
                    return false;
                }
                *revision_id = destination_native.revision.id.clone();
                true
            }
            _ => true,
        }
    };
    let facts = destination;
    let mut provenance_sources = BTreeSet::<(String, semantic::EvidenceKind)>::new();
    for raw_id in &captured.fused_proofs {
        provenance_sources.insert((raw_id.clone(), semantic::EvidenceKind::SemanticReference));
    }
    let mut symbol_rows = Vec::new();
    for mut symbol in facts.symbols {
        let raw = filtered_index.proof(&symbol.provenance_id)?;
        if let Some(claim) = raw {
            let doc = native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language,
                path: claim.path,
            };
            if let Some(targets) = captured_targets.get(&(doc, symbol.key.symbol.clone())) {
                symbol.declarations = targets.clone();
            }
        }
        if symbol.declarations.iter_mut().all(&retarget) {
            provenance_sources.insert((
                symbol.provenance_id.clone(),
                semantic::EvidenceKind::DeclarationBinding,
            ));
            symbol_rows.push(symbol);
        } else if let Some(claim) = filtered_index.proof(&symbol.provenance_id)? {
            omitted.insert(Some(native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language,
                path: claim.path,
            }));
        }
    }
    let mut bindings = Vec::new();
    for binding in facts.declaration_bindings {
        if binding.syntax_id.is_some()
            && captured_declarations.get(&binding.provenance_id) == binding.syntax_id.as_ref()
        {
            provenance_sources.insert((
                binding.provenance_id.clone(),
                semantic::EvidenceKind::DeclarationBinding,
            ));
            bindings.push(binding);
        }
    }
    let old_targets: BTreeMap<_, _> = captured
        .call_bindings
        .iter()
        .filter_map(|binding| {
            captured
                .reference_claims
                .get(&binding.provenance_id)
                .map(|reference| {
                    (
                        binding.provenance_id.clone(),
                        (
                            reference.resolution,
                            reference.declared_target.clone(),
                            reference.candidates.clone(),
                        ),
                    )
                })
        })
        .collect();
    // The captured join's classification of each call and the target it was made against.
    let captured_dispatch: BTreeMap<_, _> = captured
        .call_bindings
        .iter()
        .map(|binding| {
            (
                binding.provenance_id.as_str(),
                (binding.dispatch, binding.declared_target.as_ref()),
            )
        })
        .collect();
    let mut calls = Vec::new();
    // Each destination call's join-time dispatch and the target it was classified against.
    let mut joined_dispatch = BTreeMap::new();
    for mut call in facts.call_bindings {
        let Some((resolution, target, candidates)) = old_targets.get(&call.provenance_id) else {
            continue;
        };
        call.resolution = *resolution;
        joined_dispatch.insert(
            call.provenance_id.clone(),
            (
                call.dispatch,
                std::mem::replace(&mut call.declared_target, target.clone()),
            ),
        );
        call.candidates = candidates.clone();
        if call
            .declared_target
            .iter_mut()
            .chain(call.candidates.iter_mut())
            .all(&retarget)
        {
            call.stale_target = matches!(
                call.declared_target,
                Some(semantic::Target::Internal { .. })
            )
            .then_some(false);
            provenance_sources.insert((
                call.provenance_id.clone(),
                semantic::EvidenceKind::SemanticReference,
            ));
            calls.push(call);
        } else {
            omitted.insert(Some(call.join.anchor.document.clone()));
        }
    }
    let mut reference_rows = BTreeMap::<String, semantic::Reference>::new();
    let mut reference_targets = BTreeMap::<String, Vec<semantic::Target>>::new();
    let mut reference_proofs = BTreeMap::<String, Vec<String>>::new();
    let mut accepted_reference_ids = BTreeMap::new();
    for (raw_id, mut reference) in facts.reference_claims {
        let Some(prior) = captured.reference_claims.get(&raw_id) else {
            continue;
        };
        ensure!(
            prior.document == reference.document && prior.range == reference.range,
            "captured reference anchor differs"
        );
        reference.roles.retain(|role| prior.roles.contains(role));
        if reference.roles.is_empty() {
            continue;
        }
        reference.resolution = prior.resolution;
        reference.declared_target = prior.declared_target.clone();
        reference.candidates = prior.candidates.clone();
        if !reference
            .declared_target
            .iter_mut()
            .chain(reference.candidates.iter_mut())
            .all(&retarget)
        {
            omitted.insert(Some(reference.document.clone()));
            continue;
        }
        reference_targets
            .entry(reference.id.clone())
            .or_default()
            .extend(
                reference
                    .declared_target
                    .iter()
                    .chain(&reference.candidates)
                    .cloned(),
            );
        reference_proofs
            .entry(reference.id.clone())
            .or_default()
            .push(raw_id.clone());
        accepted_reference_ids.insert(raw_id.clone(), reference.id.clone());
        provenance_sources.insert((raw_id, semantic::EvidenceKind::SemanticReference));
        if let Some(row) = reference_rows.get_mut(&reference.id) {
            let roles: BTreeSet<_> = row.roles.iter().chain(&reference.roles).copied().collect();
            row.roles = roles.into_iter().collect();
            if row.roles.contains(&semantic::Role::Definition) {
                row.site = semantic::ReferenceSite::Declaration;
            }
        } else {
            reference_rows.insert(reference.id.clone(), reference);
        }
    }
    let mut references = Vec::new();
    let mut additional_reference_proofs = BTreeMap::new();
    for (id, mut reference) in reference_rows {
        let mut targets = reference_targets.remove(&id).unwrap_or_default();
        targets.sort_by_key(|target| {
            native_ids::canonical(&serde_json::to_value(target).expect("serializable target"))
        });
        targets.dedup();
        reference.declared_target = None;
        reference.candidates.clear();
        if targets.len() > 1 {
            reference.resolution = semantic::Resolution::Ambiguous;
            reference.candidates = targets;
        } else if let Some(target) = targets.pop() {
            reference.resolution = if matches!(target, semantic::Target::Internal { .. }) {
                semantic::Resolution::Resolved
            } else {
                semantic::Resolution::External
            };
            reference.declared_target = Some(target);
        } else {
            reference.resolution = semantic::Resolution::Unresolved;
        }
        for raw_id in reference_proofs.remove(&id).unwrap_or_default() {
            additional_reference_proofs.insert(
                raw_id,
                if reference.resolution == semantic::Resolution::Ambiguous {
                    3
                } else {
                    1
                },
            );
        }
        references.push(reference);
    }
    let references_by_id: BTreeMap<_, _> =
        references.iter().map(|row| (row.id.clone(), row)).collect();
    let target_is_method = |syntax_id: &str, document: &native::DocumentKey| {
        Ok(r2
            .declarations_by_id(syntax_id)
            .iter()
            .any(|d| d.document == *document && d.kind == "method"))
    };
    let mut rejoin_failure = None;
    calls.retain_mut(|call| {
        let Some(reference) = accepted_reference_ids
            .get(&call.provenance_id)
            .and_then(|id| references_by_id.get(id))
        else {
            return false;
        };
        call.resolution = reference.resolution;
        call.declared_target = reference.declared_target.clone();
        call.candidates = reference.candidates.clone();
        call.stale_target = call.declared_target.as_ref().and_then(|target| {
            matches!(target, semantic::Target::Internal { .. }).then_some(false)
        });
        let destination = joined_dispatch
            .get(&call.provenance_id)
            .map(|(dispatch, target)| (*dispatch, target.as_ref()));
        let original = captured_dispatch.get(call.provenance_id.as_str()).copied();
        // The reference for reader-visible evidence applies the #557 demotion itself (#1279).
        match rejoined_dispatch(destination, original, call, target_is_method, |_| Ok(false)) {
            Ok(dispatch) => call.dispatch = dispatch,
            Err(error) => rejoin_failure = Some(error),
        }
        true
    });
    if let Some(error) = rejoin_failure {
        return Err(error);
    }
    let eligible_proofs: BTreeSet<_> = filtered
        .claims
        .iter()
        .map(|claim| claim.proof_id.as_str())
        .collect();
    let mut relationships = Vec::new();
    // Changed B claims need not survive the destination producer join. Its measured
    // same-ID declaration must still independently pass the r2 retarget check.
    for mut relationship in captured
        .type_relationships
        .iter()
        .filter(|row| eligible_proofs.contains(row.provenance_id.as_str()))
        .cloned()
    {
        if retarget(&mut relationship.source) && retarget(&mut relationship.target) {
            provenance_sources.insert((
                relationship.provenance_id.clone(),
                semantic::EvidenceKind::TypeRelationship,
            ));
            relationships.push(relationship);
        } else if let Some(claim) = filtered_index.proof(&relationship.provenance_id)? {
            omitted.insert(Some(native::DocumentKey {
                source_set_id: destination_native.revision.source_set_id.clone(),
                language: claim.language,
                path: claim.path,
            }));
        }
    }
    let mut provenance = Vec::new();
    // Freshness rule 2 (as amended, #1327): only the documents of the producer's languages,
    // not the revision identity or the source-set-wide manifest, decide `possiblyStale`.
    let producer_manifest = |revision: &native::Revision| {
        crate::batch_descriptor::ProducerManifestHasher::of(
            &revision.source_set_id,
            &producer.languages,
            &revision.documents,
        )
    };
    let producer_documents_unchanged = producer_manifest(&captured_native.revision)
        == producer_manifest(&destination_native.revision);
    let mut ids = BTreeMap::new();
    for (raw_id, kind) in provenance_sources {
        let claim = raw_index
            .proof(&raw_id)?
            .context("missing raw fact for joined provenance")?;
        raw_index.check_proof(&raw_id, &claim.record_kind, &claim.fact_digest)?;
        let doc = native::DocumentKey {
            source_set_id: destination_native.revision.source_set_id.clone(),
            language: claim.language.clone(),
            path: claim.path.clone(),
        };
        let captured_basis = &basis
            .basis_for(&doc)?
            .context("missing immutable captured basis")?;
        ensure!(
            captured_basis.revision_id == captured_native.revision.id
                && captured_basis.artifact_hash == claim.artifact_hash
                && captured_basis.source_manifest_hash == inventory.source_manifest_hash
                && captured_basis.producer_id == producer.id
                && captured_basis.language
                    == serde_json::from_value::<semantic::Language>(serde_json::json!(
                        doc.language
                    ))?,
            "captured semantic basis differs from authenticated inventory"
        );
        let dst = r2.document(&doc).context("missing destination document")?;
        let freshness = if captured_native
            .revision
            .documents
            .iter()
            .find(|old| old.key == doc)
            .is_none_or(|old| old.content_hash != dst.content_hash)
        {
            semantic::Freshness::Stale
        } else if !producer_documents_unchanged
            || captured_basis.toolchain_hash != destination_native.revision.toolchain_hash
            || captured_basis.config_hash != destination_native.revision.config_hash
            || captured_basis.dependency_hash != destination_native.revision.dependency_hash
            || captured_basis.producer_version != producer.version
            || captured_basis.producer_hash != producer.executable_hash
            || !producer.languages.contains(&captured_basis.language)
        {
            semantic::Freshness::PossiblyStale
        } else {
            semantic::Freshness::Fresh
        };
        let id = rejoin_provenance_id(&raw_id, &destination_native.revision.id, kind);
        ids.insert((raw_id.clone(), kind), id.clone());
        provenance.push(semantic::Provenance {
            id,
            producer_id: producer.id.clone(),
            document: doc,
            revision_id: destination_native.revision.id.clone(),
            content_hash: dst.content_hash.clone(),
            evidence_kind: kind,
            basis: Some(captured_basis.clone()),
            freshness,
            derived_from: Some(semantic::DerivedFrom {
                provenance_id: raw_id,
                record_kind: serde_json::from_value(serde_json::json!(claim.record_kind))?,
                fact_digest: claim.fact_digest.clone(),
            }),
        });
    }
    let mut outcomes = BTreeMap::<String, usize>::new();
    for claim in inventory
        .claims
        .iter()
        .filter(|c| c.record_kind != "symbolInformation")
    {
        outcomes.insert(claim.proof_id.clone(), 2);
    }
    for raw_id in &unsupported_claims {
        outcomes.insert(raw_id.clone(), 4);
    }
    for row in &bindings {
        if row.syntax_id.is_some() {
            outcomes.insert(row.provenance_id.clone(), 1);
        }
    }
    for row in &references {
        outcomes.insert(
            row.provenance_id.clone(),
            if row.resolution == semantic::Resolution::Ambiguous {
                3
            } else {
                1
            },
        );
    }
    for row in &calls {
        outcomes.insert(
            row.provenance_id.clone(),
            if row.resolution == semantic::Resolution::Ambiguous {
                3
            } else {
                1
            },
        );
    }
    for (raw_id, outcome) in &additional_reference_proofs {
        outcomes.insert(raw_id.clone(), *outcome);
    }
    for row in &relationships {
        outcomes.insert(row.provenance_id.clone(), 1);
    }
    for row in &mut symbol_rows {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::DeclarationBinding,
        )]
            .clone();
    }
    for row in &mut bindings {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::DeclarationBinding,
        )]
            .clone();
    }
    for row in &mut references {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::SemanticReference,
        )]
            .clone();
    }
    for row in &mut calls {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::SemanticReference,
        )]
            .clone();
    }
    for row in &mut relationships {
        row.provenance_id = ids[&(
            row.provenance_id.clone(),
            semantic::EvidenceKind::TypeRelationship,
        )]
            .clone();
    }
    let mut coverage = Vec::new();
    for doc in &destination_native.revision.documents {
        let language: semantic::Language =
            serde_json::from_value(serde_json::json!(doc.key.language))?;
        if !producer.languages.contains(&language) {
            continue;
        }
        let requested = requested.contains(&doc.key);
        let failed = failed.contains(&doc.key);
        let excluded = exclusions.get(&(doc.key.language.as_str(), doc.key.path.as_str()));
        let cap = inventory
            .capped_documents
            .get(&(doc.key.language.clone(), doc.key.path.clone()))
            .copied();
        let capped = cap.is_some();
        let reasons = semantic::CoverageReasons::new(
            unsupported_documents
                .get(&doc.key)
                .copied()
                .unwrap_or_default(),
            capped,
            omitted.contains(&Some(doc.key.clone())),
            captured.diagnostics.contains_key(&doc.key),
        );
        let partial = excluded == Some(&"unsupported") || !reasons.is_empty();
        let state = if !requested {
            semantic::CoverageState::NotRequested
        } else if failed {
            semantic::CoverageState::Failed
        } else if excluded == Some(&"unsupported") {
            semantic::CoverageState::Omitted
        } else if partial {
            semantic::CoverageState::Partial
        } else {
            semantic::CoverageState::Complete
        };
        let mut observed = BTreeSet::new();
        if bindings
            .iter()
            .any(|b| b.join.anchor.document == doc.key && b.syntax_id.is_some())
        {
            observed.insert(semantic::Role::Definition);
        }
        for row in references.iter().filter(|r| r.document == doc.key) {
            observed.extend(row.roles.iter().copied());
        }
        if calls.iter().any(|b| b.join.anchor.document == doc.key) {
            observed.insert(semantic::Role::Call);
        }
        if relationships.iter().any(|r| {
            provenance
                .iter()
                .any(|p| p.id == r.provenance_id && p.document == doc.key)
        }) {
            observed.insert(semantic::Role::Type);
        }
        let supported_roles = [
            semantic::Role::Definition,
            semantic::Role::Read,
            semantic::Role::Write,
            semantic::Role::Call,
            semantic::Role::Type,
            semantic::Role::Import,
            semantic::Role::Alias,
        ]
        .into_iter()
        .filter(|r| language != semantic::Language::Java || *r != semantic::Role::Alias)
        .collect();
        coverage.push(semantic::Coverage {
            producer_id: producer.id.clone(),
            language,
            source_set_id: doc.key.source_set_id.clone(),
            document_path: doc.key.path.clone(),
            revision_id: destination_native.revision.id.clone(),
            requested,
            selected: requested && state != semantic::CoverageState::Omitted,
            state,
            supported_roles,
            observed_roles: observed.into_iter().collect(),
            diagnostic: if failed {
                Some(
                    if excluded == Some(&"failed") {
                        "excluded_failed_by_producer"
                    } else {
                        "unmeasurable_source_anchor"
                    }
                    .into(),
                )
            } else if excluded == Some(&"unsupported") {
                Some("excluded_by_producer".into())
            } else if reasons.native_gaps() > 0 {
                Some("unsupported_native_gap".into())
            } else if let Some(cap) = cap {
                Some(cap.diagnostic().into())
            } else if partial {
                Some("unmeasurable_internal_target".into())
            } else {
                None
            },
            reasons: (state == semantic::CoverageState::Partial).then_some(reasons),
        });
    }
    let mut language_counts: BTreeMap<semantic::Language, [usize; 5]> = BTreeMap::new();
    let mut reasons: BTreeMap<semantic::Language, LanguageReasons> = BTreeMap::new();
    for row in &coverage {
        reasons.entry(row.language).or_default().add(row);
        let mut counts = [0usize; 5];
        for claim in inventory.claims.iter().filter(|c| {
            c.language
                == serde_json::to_value(row.language)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default()
                && c.path == row.document_path
                && c.record_kind != "symbolInformation"
        }) {
            counts[0] += 1;
            let outcome = if row.state == semantic::CoverageState::Failed {
                2
            } else {
                *outcomes
                    .get(&claim.proof_id)
                    .context("missing rejoin claim outcome")?
            };
            counts[outcome] += 1;
        }
        ensure!(
            counts[0] == counts[1] + counts[2] + counts[3] + counts[4],
            "incomplete semantic import claim tally"
        );
        let aggregate = language_counts.entry(row.language).or_default();
        for (total, count) in aggregate.iter_mut().zip(counts) {
            *total += count;
        }
    }
    let languages: Vec<_> = language_counts
        .into_iter()
        .map(|(language, counts)| {
            serde_json::json!({
                "language":language,"requested":counts[0],"matched":counts[1],"skipped":counts[2],
                "ambiguous":counts[3],"unsupported":counts[4],
                "partialReasons":reasons.remove(&language).unwrap_or_default().json()
            })
        })
        .collect();
    #[allow(unused_mut)]
    let mut report = serde_json::json!({"producerId":producer.id,"sourceSetId":destination_native.revision.source_set_id,
        "capturedRevision":captured_native.revision.id,"destinationRevision":destination_native.revision.id,
        "languages":languages});
    #[cfg(feature = "test-support")]
    if let Ok(bytes) = std::env::var("TRELLIS_TEST_SEMANTIC_REPORT_PADDING")
        && let Ok(bytes) = bytes.parse::<usize>()
        && bytes <= 1024 * 1024
    {
        report["padding"] = serde_json::Value::String("x".repeat(bytes));
    }
    ensure!(
        serde_json::to_vec(&report)?.len() <= 1024 * 1024,
        "semantic import report exceeds response bound"
    );
    for row in coverage {
        sink.coverage(row)?;
    }
    for row in provenance {
        sink.provenance(row)?;
    }
    for row in symbol_rows {
        sink.symbol(row)?;
    }
    for row in bindings {
        sink.declaration_binding(row)?;
    }
    for row in references {
        sink.reference(row)?;
    }
    for row in calls {
        sink.call_binding(row)?;
    }
    for row in relationships {
        sink.type_relationship(row)?;
    }
    Ok(report)
}

pub(crate) fn manifest_hash(revision: &native::Revision) -> String {
    native_ids::digest(
        b"",
        &native_ids::canonical(&serde_json::json!(
            revision
                .documents
                .iter()
                .map(|d| serde_json::json!({"document":d.key,"contentHash":d.content_hash}))
                .collect::<Vec<_>>()
        )),
    )
}
