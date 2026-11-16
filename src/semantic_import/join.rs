//! The join driver: occurrences, references, calls and their settlement.
#[cfg(test)]
use super::work::complexity_work;
#[cfg(feature = "test-support")]
use super::work::count_publication_work;
use super::{
    TargetCallable,
    coordinates::{CoordinateMaps, InventoryIndex},
    dispatch::{CallShape, MeasuredOccurrence, call_dispatch, call_shape},
    join_store::{
        DocumentJoinState, GlobalDiscoverySink, GlobalJoinReads, IndexedJoinStore, JoinSink,
        JoinStore, OccurrenceRow,
    },
    keys::{claim_target, defining_key, scoped_declaration_targets, scoped_symbol_key, using_key},
    measured::{MeasuredFacts, ScopedKey},
    memory::{MemoryJoinSink, MemoryJoinStore},
    relationships::join_type_relationships,
    role_sites::RoleSites,
    settled_dispatch, staged,
};
use crate::{
    native_evidence as native, native_ids,
    scip_raw::{CapturedClaim, CapturedScipInventory},
    semantic_evidence as semantic,
};
use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default)]
pub struct JoinedFacts {
    pub symbols: Vec<semantic::Symbol>,
    pub declaration_bindings: Vec<semantic::DeclarationBinding>,
    pub references: Vec<semantic::Reference>,
    pub reference_claims: BTreeMap<String, semantic::Reference>,
    pub call_bindings: Vec<semantic::CallBinding>,
    pub type_relationships: Vec<semantic::TypeRelationship>,
    pub joins: Vec<(String, semantic::Join)>,
    pub diagnostics: BTreeMap<native::DocumentKey, BTreeSet<String>>,
    /// Raw proofs of a reused document fused into another proof's reference row: they keep
    /// their minted proof but own no row.
    pub fused_proofs: BTreeSet<String>,
    /// The proofs of a reference row that a fused call proof also claims.
    pub fused_calls: BTreeSet<String>,
}

/// Classify admitted occurrence claims against a measured native snapshot. The caller owns
/// captured proof persistence and publication; this function never consults mutable files.
pub fn join_inventory(
    inventory: &CapturedScipInventory,
    native: &native::Artifact,
    sources: &BTreeMap<native::DocumentKey, Vec<u8>>,
    admitted_encoding: u8,
) -> Result<JoinedFacts> {
    let raw_index = InventoryIndex::new(inventory)?;
    let coordinates = CoordinateMaps::new(inventory, native, sources, admitted_encoding)?;
    let measured = MeasuredFacts::new(native, sources)?;
    let store = MemoryJoinStore::new(&raw_index, &coordinates, &measured, sources);
    join_into_facts(admitted_encoding, &store)
}

pub(super) fn join_into_facts<S: IndexedJoinStore>(
    admitted_encoding: u8,
    store: &S,
) -> Result<JoinedFacts> {
    let mut sink = MemoryJoinSink::default();
    join_inventory_indexed(admitted_encoding, store, &mut sink)?;
    Ok(sink.into_facts())
}

fn join_inventory_indexed<S: IndexedJoinStore, K: JoinSink>(
    admitted_encoding: u8,
    store: &S,
    out: &mut K,
) -> Result<()> {
    let mut index = build_join_index(admitted_encoding, store)?;
    emit_join_symbols(store, &index, out)?;
    join_occurrences(admitted_encoding, store, &mut index, out)?;
    settle_join_references(store, &mut index, out)?;
    join_type_relationships(store, &mut index, out)
}

fn measure_occurrence<S: JoinStore>(
    store: &S,
    claim: &CapturedClaim,
    doc: &native::DocumentKey,
    symbol: &str,
    roles: u64,
    range: native::Range,
    operation: &mut S::Operation,
) -> Result<Option<MeasuredOccurrence>> {
    // Valid empty module markers retain their raw proof and declared symbol,
    // but cannot author a measured name, reference or call association.
    if range.start == range.end {
        return Ok(None);
    }
    let binding = store.join_reserved(
        doc,
        range.clone(),
        semantic::JoinKind::DeclarationName,
        operation,
    )?;
    let reference =
        store.join_reserved(doc, range.clone(), semantic::JoinKind::Reference, operation)?;
    let call = store.join_reserved(doc, range.clone(), semantic::JoinKind::Callee, operation)?;
    let mut callable = TargetCallable::Other;
    let compatible_syntax_id = if roles & 1 != 0 && binding.status == semantic::JoinStatus::Exact {
        let candidate = store
            .declaration_of(doc, &binding.candidate_ids[0])?
            .context("missing exact native declaration")?;
        let parent = TargetCallable::parent(store, &candidate)?;
        callable = TargetCallable::of(&candidate, parent.as_deref());
        // A stated SCIP kind must agree with the measured native kind. An unspecified kind
        // (`0`, as scip-typescript 0.4.0 emits for every symbol) states nothing to conflict,
        // so it defers to the measured native kind of this exact, authenticated target (#473).
        let compatible = store
            .symbol_kind(&doc.language, &doc.path, symbol)?
            .is_some_and(|kind| match candidate.kind.as_str() {
                _ if kind == 0 => true,
                "type" => matches!(kind, 7 | 11 | 21),
                // Native JavaScript represents a class constructor as a method. SCIP can
                // state either its constructor semantics or its method declaration shape.
                "method"
                    if matches!(
                        candidate.document.language.as_str(),
                        "javascript" | "typescript"
                    ) && candidate.name.as_deref() == Some("constructor") =>
                {
                    matches!(kind, 9 | 26)
                }
                // rust-analyzer 1.98.1 states `TraitMethod` for a bodiless trait method and
                // `StaticMethod` for an associated function without `self` (#557).
                "method" => matches!(kind, 26 | 70 | 80),
                _ => true,
            });
        if compatible {
            store.reserve_operation(operation, || Ok(binding.candidate_ids[0].len() as u64))?;
        }
        compatible.then(|| binding.candidate_ids[0].clone())
    } else {
        None
    };
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            (std::mem::size_of::<OccurrenceRow>() - 3 * std::mem::size_of::<semantic::Join>())
                as u64,
            claim.captured_proof_id.len() as u64,
            claim.fact_digest.len() as u64,
            staged::sizes::document(doc)?,
            symbol.len() as u64,
        ])
    })?;
    Ok(Some(MeasuredOccurrence {
        row: OccurrenceRow {
            proof_id: claim.captured_proof_id.clone(),
            fact_digest: claim.fact_digest.clone(),
            document: doc.clone(),
            symbol: symbol.to_owned(),
            roles,
            binding,
            reference,
            call,
        },
        compatible_syntax_id,
        callable,
    }))
}

pub(super) fn discover_join_claim<S: JoinStore, G: GlobalDiscoverySink>(
    admitted_encoding: u8,
    store: &S,
    global: &mut G,
    claim: &CapturedClaim,
    operation: &mut S::Operation,
) -> Result<Option<(native::Range, OccurrenceRow)>> {
    #[cfg(feature = "test-support")]
    let revision_id = store.revision_id();
    store.charge(1)?;
    store.lookup_proof(
        &claim.captured_proof_id,
        &claim.envelope.record_kind,
        &claim.fact_digest,
    )?;
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            std::mem::size_of::<native::DocumentKey>() as u64,
            store.source_set_id().len() as u64,
            claim.language.len() as u64,
            claim.envelope.relative_path.len() as u64,
        ])
    })?;
    let doc = native::DocumentKey {
        source_set_id: store.source_set_id().to_owned(),
        language: claim.language.clone(),
        path: claim.envelope.relative_path.clone(),
    };
    store
        .bytes_of(&doc)?
        .context("missing admitted claim document")?;
    let target = claim_target(store.source_set_id(), claim);
    if claim.envelope.record_kind == "symbolInformation" {
        if let Some(symbol) = claim.envelope.raw["symbol"].as_str() {
            global.symbol_presence(symbol, target.as_ref(), false, true)?;
        }
    } else if claim.envelope.record_kind == "occurrence" {
        let symbol = claim.envelope.raw["symbol"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("missing occurrence symbol")?;
        let range = store.occurrence_range(claim, admitted_encoding)?;
        #[cfg(feature = "test-support")]
        count_publication_work(|work| {
            if work.destination_revision == revision_id {
                work.destination_role_parses += 1;
            } else {
                work.original_role_parses += 1;
            }
        });
        let roles = claim.envelope.raw["symbolRoles"]
            .as_u64()
            .context("invalid occurrence roles")?;
        let _syntax_kind = claim.envelope.raw["syntaxKind"]
            .as_i64()
            .context("invalid syntax kind")?;
        let declaration = roles & 1 != 0;
        if declaration {
            global.symbol_presence(symbol, target.as_ref(), true, false)?;
        }
        let Some(measured) =
            measure_occurrence(store, claim, &doc, symbol, roles, range.clone(), operation)?
        else {
            return Ok(None);
        };
        if roles & 1 != 0 && measured.row.binding.status == semantic::JoinStatus::Exact {
            let key = defining_key(symbol, &doc, target.as_ref());
            store.reserve_operation(operation, || {
                staged::sizes::sum(&[
                    std::mem::size_of::<ScopedKey>() as u64,
                    symbol.len() as u64,
                    key.1.as_ref().map_or(Ok(0), staged::sizes::document)?,
                    std::mem::size_of::<native::DocumentKey>() as u64,
                    staged::sizes::document(&doc)?,
                    std::mem::size_of::<String>() as u64,
                    measured.row.binding.candidate_ids[0].len() as u64,
                ])
            })?;
            global.exact_definition(
                key,
                doc.clone(),
                measured.row.binding.candidate_ids[0].clone(),
                measured.compatible_syntax_id.is_some(),
                measured.callable,
            )?;
        }
        return Ok(Some((range, measured.row)));
    }
    Ok(None)
}

pub(super) fn build_join_index<S: IndexedJoinStore>(
    admitted_encoding: u8,
    store: &S,
) -> Result<S::Index> {
    let mut index = store.new_index();
    for claim in store.claims() {
        let claim = claim?;
        let mut operation = store.operation()?;
        if let Some((range, row)) =
            discover_join_claim(admitted_encoding, store, &mut index, &claim, &mut operation)?
        {
            index.put_type_use(&row.document, range.start, range.end, &row.symbol)?;
            index.put_occurrence(row)?;
        }
    }
    Ok(index)
}

pub(super) fn emit_join_symbols<S: JoinStore, G: GlobalJoinReads, K: JoinSink>(
    store: &S,
    index: &G,
    out: &mut K,
) -> Result<()> {
    let source_set_id = store.source_set_id();
    for claim in store.claims() {
        let claim = claim?;
        if claim.envelope.record_kind != "symbolInformation" {
            continue;
        }
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || {
            staged::sizes::sum(&[
                std::mem::size_of::<native::DocumentKey>() as u64,
                source_set_id.len() as u64,
                claim.language.len() as u64,
                claim.envelope.relative_path.len() as u64,
            ])
        })?;
        let doc = native::DocumentKey {
            source_set_id: source_set_id.to_owned(),
            language: claim.language.clone(),
            path: claim.envelope.relative_path.clone(),
        };
        let key = claim.envelope.raw["symbol"]
            .as_str()
            .filter(|s| !s.is_empty())
            .context("missing symbol key")?;
        let scoped = defining_key(key, &doc, claim_target(source_set_id, &claim).as_ref());
        let declarations = scoped_declaration_targets(store, index, &scoped, &mut operation)?;
        let display_name = claim.envelope.raw["displayName"]
            .as_str()
            .filter(|s| !s.is_empty());
        store.reserve_operation(&mut operation, || {
            staged::sizes::sum(&[
                std::mem::size_of::<semantic::Symbol>() as u64,
                key.len() as u64,
                scoped.1.as_ref().map_or(Ok(0), staged::sizes::document)?,
                display_name.map_or(0, |s| s.len() as u64),
                claim.captured_proof_id.len() as u64,
            ])
        })?;
        out.symbol(semantic::Symbol {
            key: scoped_symbol_key(&scoped),
            display_name: display_name.map(str::to_owned),
            declarations,
            provenance_id: claim.captured_proof_id.clone(),
        })?;
    }
    Ok(())
}

fn join_occurrences<S: IndexedJoinStore, K: JoinSink>(
    admitted_encoding: u8,
    store: &S,
    index: &mut S::Index,
    out: &mut K,
) -> Result<()> {
    let mut role_sites = None;
    for ordinal in 0..index.occurrence_count() {
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || index.occurrence_clone_bytes(ordinal))?;
        let row = index.occurrence(ordinal)?;
        join_occurrence(
            admitted_encoding,
            store,
            index,
            out,
            row,
            &mut role_sites,
            &mut operation,
        )?;
    }
    Ok(())
}

/// One measured occurrence with its claim, as the reference and call steps of
/// [`join_occurrence`] read it.
#[derive(Clone, Copy)]
struct Occurrence<'a> {
    revision_id: &'a str,
    claim: &'a CapturedClaim,
    doc: &'a native::DocumentKey,
    symbol: &'a str,
    roles: u64,
    binding: &'a semantic::Join,
    reference: &'a semantic::Join,
    call: &'a semantic::Join,
    /// The invocation the producer's enclosing range exactly joins, if any.
    invocation: &'a Option<semantic::Join>,
    /// The measured call this use occurrence names (#444), if any.
    witness: Option<&'a semantic::Join>,
}

/// What an occurrence's symbol resolves to: one asserted target is `resolved`, several are
/// `candidates`.
struct Targets {
    resolution: semantic::Resolution,
    resolved: Option<semantic::Target>,
    candidates: Vec<semantic::Target>,
}

pub(super) fn join_occurrence<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    admitted_encoding: u8,
    store: &S,
    index: &mut I,
    out: &mut K,
    measured: OccurrenceRow,
    role_sites: &mut Option<(native::DocumentKey, RoleSites, S::Operation)>,
    operation: &mut S::Operation,
) -> Result<()> {
    let revision_id = store.revision_id();
    let OccurrenceRow {
        proof_id,
        fact_digest,
        document: doc,
        symbol,
        roles,
        binding,
        reference,
        call,
    } = measured;
    let claim = store.claim(&proof_id, "occurrence", &fact_digest)?;
    let (key, targets) = resolve_targets(store, index, &claim, &symbol, &doc, operation)?;
    if roles & 1 != 0 {
        emit_declaration_binding(store, out, &claim, &binding, &key, operation)?;
    }
    out.join(&claim.captured_proof_id, &reference)?;
    let invocation =
        enclosing_invocation(admitted_encoding, store, &claim, &doc, roles, operation)?;
    // The measured call this use occurrence names (#444). SCIP has no call role, and
    // scip-typescript, scip-java and scip-python never give a reference an enclosing range,
    // so the occurrence's exact span on one measured callee identifies the call by itself.
    // A producer-supplied enclosing range that exactly joins an invocation must join that
    // same call; one that joins none is absent, and the exact callee span alone decides.
    let witness = match &invocation {
        Some(j) => (j.status == semantic::JoinStatus::Exact
            && (call.status != semantic::JoinStatus::Exact
                || call.candidate_ids == j.candidate_ids))
            .then_some(j),
        None => (roles & 1 == 0 && call.status == semantic::JoinStatus::Exact).then_some(&call),
    };
    let occurrence = Occurrence {
        revision_id,
        claim: &claim,
        doc: &doc,
        symbol: &symbol,
        roles,
        binding: &binding,
        reference: &reference,
        call: &call,
        invocation: &invocation,
        witness,
    };
    if reference.status == semantic::JoinStatus::Exact {
        join_reference(
            store, index, out, occurrence, &targets, role_sites, operation,
        )?;
    }
    if roles & 1 == 0 {
        join_call(store, index, out, occurrence, targets, operation)?;
    }
    Ok(())
}

/// The occurrence's symbol key and the targets it asserts.
fn resolve_targets<S: JoinStore, I: DocumentJoinState>(
    store: &S,
    index: &I,
    claim: &CapturedClaim,
    symbol: &str,
    doc: &native::DocumentKey,
    operation: &mut S::Operation,
) -> Result<(semantic::SymbolKey, Targets)> {
    let scoped = using_key(
        index,
        symbol,
        doc,
        claim_target(store.source_set_id(), claim).as_ref(),
    )?;
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            std::mem::size_of::<semantic::SymbolKey>() as u64,
            symbol.len() as u64,
            scoped.1.as_ref().map_or(Ok(0), staged::sizes::document)?,
        ])
    })?;
    let key = scoped_symbol_key(&scoped);
    let mut asserted = scoped_declaration_targets(store, index, &scoped, operation)?;
    // A symbol the artifact never declares nor describes is external, unless a source of this
    // language wasn't read (#576) or was admitted over a per-document cap with no claims (#634):
    // the producer saw it, but its claims are dropped.
    if asserted.is_empty()
        && key.scope == semantic::SymbolScope::Global
        && index.presence(symbol, None)? == (false, false)
        && !store.has_unclaimed_source(&doc.language)?
    {
        store.reserve_operation(operation, || {
            staged::sizes::sum(&[
                staged::sizes::symbol_key(&key)?,
                4 * std::mem::size_of::<semantic::Target>() as u64,
            ])
        })?;
        asserted.push(semantic::Target::External {
            symbol: key.clone(),
        });
    }
    store.reserve_operation(operation, || staged::target_size::sort(&asserted))?;
    asserted.sort_by_key(|t| {
        native_ids::canonical(&serde_json::to_value(t).expect("serializable target"))
    });
    asserted.dedup();
    let (resolution, resolved, candidates) = match asserted.len() {
        0 => (semantic::Resolution::Unresolved, None, vec![]),
        1 => {
            let item = asserted.pop().unwrap();
            let status = if matches!(item, semantic::Target::Internal { .. }) {
                semantic::Resolution::Resolved
            } else {
                semantic::Resolution::External
            };
            (status, Some(item), vec![])
        }
        _ => (semantic::Resolution::Ambiguous, None, asserted),
    };
    Ok((
        key,
        Targets {
            resolution,
            resolved,
            candidates,
        },
    ))
}

/// A definition occurrence: its binding join and the declaration binding row.
fn emit_declaration_binding<S: JoinStore, K: JoinSink>(
    store: &S,
    out: &mut K,
    claim: &CapturedClaim,
    binding: &semantic::Join,
    key: &semantic::SymbolKey,
    operation: &mut S::Operation,
) -> Result<()> {
    out.join(&claim.captured_proof_id, binding)?;
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            (std::mem::size_of::<semantic::DeclarationBinding>()
                - std::mem::size_of::<semantic::Join>()) as u64,
            staged::sizes::join(binding)?,
            claim.captured_proof_id.len() as u64,
            if binding.status == semantic::JoinStatus::Exact {
                staged::sizes::sum(&[
                    binding.candidate_ids[0].len() as u64,
                    std::mem::size_of::<semantic::SymbolKey>() as u64,
                    staged::sizes::symbol_key(key)?,
                ])?
            } else {
                0
            },
        ])
    })?;
    out.declaration_binding(semantic::DeclarationBinding {
        syntax_id: (binding.status == semantic::JoinStatus::Exact)
            .then(|| binding.candidate_ids[0].clone()),
        symbols: if binding.status == semantic::JoinStatus::Exact {
            vec![key.clone()]
        } else {
            vec![]
        },
        join: binding.clone(),
        provenance_id: claim.captured_proof_id.clone(),
    })?;
    Ok(())
}

/// The invocation a use occurrence's enclosing range exactly joins, if the producer gave one.
fn enclosing_invocation<S: JoinStore>(
    admitted_encoding: u8,
    store: &S,
    claim: &CapturedClaim,
    doc: &native::DocumentKey,
    roles: u64,
    operation: &mut S::Operation,
) -> Result<Option<semantic::Join>> {
    let invocation = if roles & 1 == 0 && !claim.envelope.raw["enclosingRange"].is_null() {
        let span = store.enclosing_occurrence_range(claim, admitted_encoding)?;
        let own = store.occurrence_range(claim, admitted_encoding)?;
        // A SCIP enclosing range encloses its own occurrence. rust-analyzer instead gives a
        // same-document reference its target's definition range (#394), which encloses some
        // other span and says nothing about a call; that is no invocation witness.
        (span.start <= own.start && own.end <= span.end)
            .then(|| store.join_reserved(doc, span, semantic::JoinKind::Invocation, operation))
            .transpose()?
            // Owner ruling 4 (2026-10-30, #394): an enclosing range that does not exactly join
            // an invocation is treated as absent. A recursive rust-analyzer call's range is its
            // target's own definition, which encloses the reference but is no call.
            .filter(|join| join.status == semantic::JoinStatus::Exact)
    } else {
        None
    };
    Ok(invocation)
}

/// An occurrence that exactly joins a measured reference: assigns its roles, records its
/// targets as reference claims and emits or merges the reference row.
fn join_reference<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &mut I,
    out: &mut K,
    occurrence: Occurrence<'_>,
    targets: &Targets,
    role_sites: &mut Option<(native::DocumentKey, RoleSites, S::Operation)>,
    operation: &mut S::Operation,
) -> Result<()> {
    let Occurrence {
        claim,
        doc,
        roles,
        reference,
        ..
    } = occurrence;
    let Targets {
        resolved,
        candidates,
        ..
    } = targets;
    let r = store
        .reference_of(doc, &reference.candidate_ids[0])?
        .context("missing indexed native reference")?;
    let declaration_site = roles & 1 != 0
        && store.has_declaration_name(doc, r.range.start, r.range.end, &r.spelling)?;
    let mut assigned = assign_roles(
        store,
        index,
        occurrence,
        &r,
        declaration_site,
        role_sites,
        operation,
    )?;
    if !assigned.is_empty() {
        for target in resolved.iter().chain(candidates.iter()) {
            store.reserve_operation(operation, || staged::sizes::target(target))?;
            index.put_reference_claim(&r.id, &claim.captured_proof_id, target.clone())?;
        }
    }
    if !assigned.is_empty() {
        let reference = measured_reference(
            store,
            occurrence,
            &r,
            declaration_site,
            &assigned,
            targets,
            operation,
        )?;
        store.reserve_operation(operation, || staged::sizes::reference(&reference))?;
        out.reference_claim(&claim.captured_proof_id, reference.clone())?;
        #[cfg(test)]
        complexity_work(|work| work.reference_map_lookups += 1);
        if let Some(slot) = out.reference_slot(&r.id)? {
            store.reserve_operation(operation, || out.reference_clone_bytes(slot))?;
            let mut prior = out.reference(slot)?;
            assigned.extend(prior.roles.iter().copied());
            store.reserve_operation(operation, || {
                Ok((assigned.len() * std::mem::size_of::<semantic::Role>()) as u64)
            })?;
            prior.roles = assigned.into_iter().collect();
            if prior.roles.contains(&semantic::Role::Definition) {
                prior.site = semantic::ReferenceSite::Declaration;
            }
            out.put_reference(Some(slot), prior)?;
        } else {
            out.put_reference(None, reference)?;
        }
    }
    Ok(())
}

/// Whether the witnessed call is the measured call at this reference.
fn reference_is_call<S: JoinStore>(
    store: &S,
    doc: &native::DocumentKey,
    r: &native::Reference,
    witness: Option<&semantic::Join>,
    operation: &mut S::Operation,
) -> Result<bool> {
    let is_call = match witness {
        Some(j) => store
            .calls_with_id(doc, &j.candidate_ids[0], operation)?
            .iter()
            .any(|c| {
                c.id == j.candidate_ids[0]
                    && c.document == *doc
                    && c.owner_syntax_id == r.owner_syntax_id
                    && c.range.start <= r.range.start
                    && r.range.end <= c.range.end
                    && c.spelling.as_deref() == Some(&r.spelling)
                    && c.callee_range
                        .as_ref()
                        .is_none_or(|callee| callee == &r.range)
            }),
        None => false,
    };
    Ok(is_call)
}

/// Keeps the role sites of the document being joined: read once per run of occurrences of
/// one document, under an operation of their own.
fn cache_role_sites<S: JoinStore>(
    store: &S,
    doc: &native::DocumentKey,
    role_sites: &mut Option<(native::DocumentKey, RoleSites, S::Operation)>,
) -> Result<()> {
    if role_sites.as_ref().is_none_or(|(owner, _, _)| owner != doc) {
        let mut cache_operation = store.operation()?;
        store.reserve_operation(&mut cache_operation, || {
            staged::sizes::sum(&[
                std::mem::size_of::<native::DocumentKey>() as u64,
                staged::sizes::document(doc)?,
            ])
        })?;
        let sites = store.role_sites_reserved(doc, &mut cache_operation)?;
        *role_sites = Some((doc.clone(), sites, cache_operation));
    }
    Ok(())
}

/// The roles this occurrence gives the measured reference `r`.
fn assign_roles<S: JoinStore, I: DocumentJoinState>(
    store: &S,
    index: &I,
    occurrence: Occurrence<'_>,
    r: &native::Reference,
    declaration_site: bool,
    role_sites: &mut Option<(native::DocumentKey, RoleSites, S::Operation)>,
    operation: &mut S::Operation,
) -> Result<BTreeSet<semantic::Role>> {
    let Occurrence {
        doc,
        symbol,
        roles,
        binding,
        witness,
        ..
    } = occurrence;
    let is_call = reference_is_call(store, doc, r, witness, operation)?;
    store.reserve_operation(operation, || {
        staged::sizes::map_entries::<semantic::Role, ()>(7)
    })?;
    let mut assigned = BTreeSet::new();
    if declaration_site {
        assigned.insert(semantic::Role::Definition);
    }
    let site = (r.range.start, r.range.end);
    cache_role_sites(store, doc, role_sites)?;
    let sites = &role_sites.as_ref().expect("role sites just cached").1;
    if roles & 2 != 0 && sites.imports.contains(&site) {
        assigned.insert(semantic::Role::Import);
    }
    if !declaration_site && sites.types.contains(&site) && {
        store.reserve_operation(operation, || {
            index.type_uses_clone_bytes(doc, site.0, site.1)
        })?;
        let keys = index.type_uses(doc, site.0, site.1)?;
        keys.len() == 1 && keys.contains(symbol)
    } {
        assigned.insert(semantic::Role::Type);
    }
    if roles & 4 != 0 {
        assigned.insert(semantic::Role::Write);
    }
    if is_call {
        assigned.insert(semantic::Role::Call);
    }
    if !declaration_site
        && roles & 4 == 0
        && roles & 2 == 0
        && (!assigned.contains(&semantic::Role::Type)
            || matches!(doc.language.as_str(), "javascript" | "typescript"))
    {
        assigned.insert(semantic::Role::Read);
    }
    if declaration_site
        && roles & 2 != 0
        && matches!(doc.language.as_str(), "javascript" | "typescript" | "rust")
        && binding.status == semantic::JoinStatus::Exact
        && store
            .declarations_with_id(doc, &binding.candidate_ids[0], operation)?
            .iter()
            .any(|d| {
                d.syntax_id == binding.candidate_ids[0]
                    && d.document == *doc
                    && d.kind == "alias"
                    && d.name_range.as_ref() == Some(&r.range)
            })
    {
        assigned.insert(semantic::Role::Alias);
    }
    Ok(assigned)
}

/// The reference row this occurrence claims for the measured reference `r`.
fn measured_reference<S: JoinStore>(
    store: &S,
    occurrence: Occurrence<'_>,
    r: &native::Reference,
    declaration_site: bool,
    assigned: &BTreeSet<semantic::Role>,
    targets: &Targets,
    operation: &mut S::Operation,
) -> Result<semantic::Reference> {
    let Occurrence {
        revision_id,
        claim,
        doc,
        ..
    } = occurrence;
    let &Targets {
        resolution,
        ref resolved,
        ref candidates,
    } = targets;
    store.reserve_operation(operation, || {
        staged::sizes::sum(&[
            std::mem::size_of::<semantic::Reference>() as u64,
            r.id.len() as u64,
            r.owner_syntax_id.len() as u64,
            staged::sizes::document(doc)?,
            revision_id.len() as u64,
            r.spelling.len() as u64,
            r.lookup_key.len() as u64,
            (assigned.len() * std::mem::size_of::<semantic::Role>()) as u64,
            resolved
                .as_ref()
                .map_or(Ok(0), staged::sizes::target_payload)?,
            staged::sizes::targets(candidates)?,
            claim.captured_proof_id.len() as u64,
        ])
    })?;
    let reference = semantic::Reference {
        id: r.id.clone(),
        owner_syntax_id: r.owner_syntax_id.clone(),
        ordinal: r.ordinal,
        document: doc.clone(),
        revision_id: revision_id.to_owned(),
        range: r.range.clone(),
        spelling: r.spelling.clone(),
        lookup_key: r.lookup_key.clone(),
        site: if declaration_site {
            semantic::ReferenceSite::Declaration
        } else {
            semantic::ReferenceSite::Use
        },
        roles: assigned.iter().copied().collect(),
        resolution,
        declared_target: resolved.clone(),
        candidates: candidates.clone(),
        provenance_id: claim.captured_proof_id.clone(),
    };
    Ok(reference)
}

/// A use occurrence: its invocation and call joins, and the call binding when the witnessed
/// call is the measured call at its reference.
fn join_call<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &mut I,
    out: &mut K,
    occurrence: Occurrence<'_>,
    targets: Targets,
    operation: &mut S::Operation,
) -> Result<()> {
    let Occurrence {
        claim,
        doc,
        reference,
        call,
        invocation,
        witness,
        ..
    } = occurrence;
    let Targets {
        resolution,
        resolved,
        candidates,
    } = targets;
    let source = store.bytes_of(doc)?.context("missing occurrence source")?;
    if let Some(join) = invocation {
        out.join(&claim.captured_proof_id, join)?;
    }
    let selected = if call.status == semantic::JoinStatus::Exact {
        call
    } else {
        invocation
            .as_ref()
            .filter(|j| j.status == semantic::JoinStatus::Exact)
            .unwrap_or(call)
    };
    store.reserve_operation(operation, || staged::sizes::join(selected))?;
    let candidate = if call.status == semantic::JoinStatus::Exact {
        call.clone()
    } else if invocation
        .as_ref()
        .is_some_and(|j| j.status == semantic::JoinStatus::Exact)
    {
        invocation.as_ref().expect("exact invocation").clone()
    } else {
        call.clone()
    };
    out.join(&claim.captured_proof_id, call)?;
    if candidate.status == semantic::JoinStatus::Exact
        && reference.status == semantic::JoinStatus::Exact
        && witness.is_some_and(|j| j.candidate_ids == candidate.candidate_ids)
    {
        let actual = store.call_of(doc, &candidate.candidate_ids[0])?;
        let owning_references =
            store.references_with_id(doc, &reference.candidate_ids[0], operation)?;
        if let Some(actual) = actual.as_deref().filter(|c| {
            let primary = &reference.anchor.range;
            c.document == *doc
                && c.range.start <= primary.start
                && primary.end <= c.range.end
                && c.callee_range.as_ref().is_none_or(|r| r == primary)
                && c.spelling.as_deref()
                    == source
                        .get(primary.start..primary.end)
                        .and_then(|b| std::str::from_utf8(b).ok())
                && owning_references.iter().any(|r| {
                    r.id == reference.candidate_ids[0]
                        && r.owner_syntax_id == c.owner_syntax_id
                        && r.range == *primary
                })
        }) {
            let dispatch = classify_dispatch(index, doc, actual, &source, &resolved, resolution)?;
            index.put_call_reference(&claim.captured_proof_id, &reference.candidate_ids[0])?;
            store.reserve_operation(operation, || {
                staged::sizes::sum(&[
                    (std::mem::size_of::<semantic::CallBinding>()
                        - std::mem::size_of::<semantic::Join>()) as u64,
                    candidate.candidate_ids[0].len() as u64,
                    claim.captured_proof_id.len() as u64,
                ])
            })?;
            out.put_call_binding(
                None,
                semantic::CallBinding {
                    call_id: Some(candidate.candidate_ids[0].clone()),
                    join: candidate,
                    resolution,
                    stale_target: resolved.as_ref().and_then(|t| {
                        matches!(t, semantic::Target::Internal { .. }).then_some(false)
                    }),
                    declared_target: resolved,
                    candidates,
                    dispatch,
                    possible_dispatch: vec![],
                    possible_dispatch_complete: false,
                    provenance_id: claim.captured_proof_id.clone(),
                },
            )?;
        }
    }
    Ok(())
}

/// How the measured call `actual` dispatches to its resolved target.
fn classify_dispatch<I: DocumentJoinState>(
    index: &I,
    doc: &native::DocumentKey,
    actual: &native::Call,
    source: &[u8],
    resolved: &Option<semantic::Target>,
    resolution: semantic::Resolution,
) -> Result<semantic::Dispatch> {
    // #470: only an exact, resolved internal target of this language is classified.
    let dispatch = match resolved {
        Some(semantic::Target::Internal {
            syntax_id,
            document,
            ..
        }) if resolution == semantic::Resolution::Resolved && document.language == doc.language => {
            match call_shape(&doc.language, actual, source) {
                CallShape::Other => semantic::Dispatch::Unknown,
                shape => call_dispatch(
                    &doc.language,
                    shape,
                    index.target_callable(document, syntax_id)?,
                ),
            }
        }
        _ => semantic::Dispatch::Unknown,
    };
    Ok(dispatch)
}

pub(super) fn settle_join_references<S: JoinStore, I: DocumentJoinState, K: JoinSink>(
    store: &S,
    index: &mut I,
    out: &mut K,
) -> Result<()> {
    for slot in 0..out.reference_count() {
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || out.reference_clone_bytes(slot))?;
        let mut reference = out.reference(slot)?;
        let (mut claims, _claims_reservation) = index.take_reference_claims(&reference.id)?;
        store.reserve_operation(&mut operation, || {
            staged::target_size::stable_sort::<(String, semantic::Target)>(claims.len())
        })?;
        claims.sort_by(|a, b| a.0.cmp(&b.0));
        if let Some((proof, _)) = claims.first() {
            store.reserve_operation(&mut operation, || Ok(proof.len() as u64))?;
            reference.provenance_id = proof.clone();
        }
        store.reserve_operation(&mut operation, || {
            staged::target_size::target_slots(claims.len())
        })?;
        let mut targets: Vec<_> = claims.into_iter().map(|(_, target)| target).collect();
        store.reserve_operation(&mut operation, || staged::target_size::sort(&targets))?;
        targets.sort_by_key(|target| {
            native_ids::canonical(&serde_json::to_value(target).expect("serializable target"))
        });
        targets.dedup();
        if targets.len() > 1 {
            reference.resolution = semantic::Resolution::Ambiguous;
            reference.declared_target = None;
            reference.candidates = targets;
        } else if let Some(target) = targets.pop() {
            reference.resolution = if matches!(target, semantic::Target::Internal { .. }) {
                semantic::Resolution::Resolved
            } else {
                semantic::Resolution::External
            };
            reference.declared_target = Some(target);
            reference.candidates.clear();
        }
        out.put_reference(Some(slot), reference)?;
    }
    for slot in 0..out.call_binding_count() {
        let mut operation = store.operation()?;
        store.reserve_operation(&mut operation, || out.call_binding_clone_bytes(slot))?;
        let mut binding = out.call_binding(slot)?;
        store.reserve_operation(&mut operation, || {
            index.call_reference_clone_bytes(&binding.provenance_id)
        })?;
        let reference_slot = match index.call_reference(&binding.provenance_id)? {
            Some(id) => {
                #[cfg(test)]
                complexity_work(|work| work.reference_map_lookups += 1);
                out.reference_slot(&id)?
            }
            None => None,
        };
        if let Some(slot_of_reference) = reference_slot {
            store.reserve_operation(&mut operation, || {
                out.reference_clone_bytes(slot_of_reference)
            })?;
            let reference = out.reference(slot_of_reference)?;
            binding.resolution = reference.resolution;
            let joined = std::mem::replace(&mut binding.declared_target, reference.declared_target);
            binding.candidates = reference.candidates;
            binding.stale_target = binding
                .declared_target
                .as_ref()
                .and_then(|t| matches!(t, semantic::Target::Internal { .. }).then_some(false));
            binding.dispatch = settled_dispatch(binding.dispatch, joined.as_ref(), &binding);
            out.put_call_binding(Some(slot), binding)?;
        }
    }
    Ok(())
}
