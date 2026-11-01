//! Pass 4 of the streaming join: one eligible document's rejoin settlement. Its captured and
//! eligible-destination joins are recomputed from the claim store and the two completed global
//! contexts, settled by the rules of the whole-corpus rejoin (`finish_rejoin`) restricted to
//! that document, validated row by row and written to `out_*`. Every rule that settlement reads
//! is document-local: a fact's raw proof, its reference and call site all belong to the
//! document; only target authority (`nat_declarations`, captured exact definitions) is global.
use super::codec;
use super::streaming::{self, CompletedGlobal};
use super::*;

/// A document's validation index and its live charge, built on the first validated row.
type ValidationIndex = std::cell::OnceCell<(semantic::NativeIndex, Reservation)>;

/// The document's validation index, built on first use: admitted to the live budget before it
/// is built (#476 accounting), and held until the cell drops with the document.
fn validation_index<'i>(
    cell: &'i ValidationIndex,
    live: &Rc<LiveBudget>,
    native: &native::Artifact,
) -> Result<&'i semantic::NativeIndex> {
    if let Some((index, _)) = cell.get() {
        return Ok(index);
    }
    let reservation = live.reserve(semantic::NativeIndex::bytes(native))?;
    let built = semantic::NativeIndex::new(native);
    Ok(&cell.get_or_init(|| (built, reservation)).0)
}

/// What one document's settlement leaves for its coverage row and report tally.
#[derive(Default)]
pub(super) struct DocumentOutcome {
    /// A rejoined fact of the document lost a target.
    pub(super) omitted: bool,
    /// The captured join reported a diagnostic for the document.
    pub(super) diagnostics: bool,
    pub(super) observed: BTreeSet<semantic::Role>,
    /// Distinct claims whose final outcome is matched, and ambiguous.
    pub(super) matched: u64,
    pub(super) ambiguous: u64,
    /// The stored provenance template the document's provenance rows share, if it has any.
    pub(super) provenance: Option<String>,
}

/// One settled reference row before its emission: the destination claim it starts from, and
/// the roles and site its claims merged into.
struct PendingReference<'a> {
    claim: &'a semantic::Reference,
    roles: Vec<semantic::Role>,
    site: Option<semantic::ReferenceSite>,
}

/// What a call copies from its emitted reference.
struct EmittedTargets {
    resolution: semantic::Resolution,
    declared_target: Option<semantic::Target>,
    candidates: Vec<semantic::Target>,
}

/// Retarget decisions already made in this document, by target identity. An optional cache
/// with its own charge: when the live budget has no room, decisions are simply recomputed.
struct Retargets {
    internal: BTreeMap<native::DocumentKey, BTreeMap<String, bool>>,
    external: BTreeMap<String, bool>,
    retained: Reservation,
}

/// Sorts targets by canonical bytes and drops duplicates, admitting the sort's scratch first.
fn canonical_targets(live: &Rc<LiveBudget>, targets: &mut Vec<semantic::Target>) -> Result<()> {
    let _scratch = live.reserve(target_size::sort(targets)?)?;
    targets.sort_by_key(|target| {
        crate::native_ids::canonical(&serde_json::to_value(target).expect("serializable target"))
    });
    targets.dedup();
    Ok(())
}

/// The owned bytes of one row's `declared`/`candidates` pair, as `sizes` counts them.
fn target_set(declared: Option<&semantic::Target>, candidates: &[semantic::Target]) -> Result<u64> {
    sizes::sum(&[
        declared.map_or(Ok(0), sizes::target_payload)?,
        sizes::targets(candidates)?,
    ])
}

/// Replaces `declared`/`candidates` with copies of `from_declared`/`from_candidates`. The
/// copies are admitted from the borrowed source before anything is cloned, while the replaced
/// values are still live (overlap); `owner`, which holds the row's charge, then releases the
/// replaced values once they are dropped. The incoming set can be larger than the one it
/// replaces: a captured internal target may survive retargeting although destination
/// discovery excluded its ineligible document.
fn adopt_targets(
    owner: &mut Reservation,
    declared: &mut Option<semantic::Target>,
    candidates: &mut Vec<semantic::Target>,
    from_declared: Option<&semantic::Target>,
    from_candidates: &[semantic::Target],
) -> Result<()> {
    let replaced = target_set(declared.as_ref(), candidates)?;
    owner.grow(target_set(from_declared, from_candidates)?)?;
    *declared = from_declared.cloned();
    *candidates = from_candidates.to_vec();
    owner.release(replaced)
}

/// A working copy of one destination row, admitted at its own size before the clone. The guard
/// follows the copy: bind it first so it is released after the copy drops, or move it to the
/// owner that keeps the copy.
fn working_copy<T: Clone>(
    live: &Rc<LiveBudget>,
    row: &T,
    bytes: impl FnOnce(&T) -> Result<u64>,
) -> Result<(Reservation, T)> {
    let reservation = live.reserve(bytes(row)?)?;
    Ok((reservation, row.clone()))
}

/// Admits one new `BTreeMap` entry whose owned key/value payload is `payload` bytes.
fn entry<K, V>(owner: &mut Reservation, payload: u64) -> Result<()> {
    owner.grow(sizes::sum(&[sizes::map_entries::<K, V>(1)?, payload])?)
}

impl StagedJoin<'_> {
    /// Whether a rejoined target still names a measured, unchanged declaration; an internal
    /// target that does is moved to the destination revision.
    fn retarget(
        &self,
        target: &mut semantic::Target,
        memo: &mut Retargets,
        owner: &mut Reservation,
    ) -> Result<bool> {
        match target {
            semantic::Target::External { symbol } => {
                if let Some(captured) = memo.external.get(&symbol.symbol) {
                    return Ok(!captured);
                }
                let captured = CompletedGlobal::captured_symbol(self.ctx, &symbol.symbol)?;
                if entry::<String, bool>(&mut memo.retained, symbol.symbol.len() as u64).is_ok() {
                    memo.external.insert(symbol.symbol.clone(), captured);
                }
                Ok(!captured)
            }
            semantic::Target::Internal {
                syntax_id,
                document,
                revision_id,
            } => {
                let known = memo
                    .internal
                    .get(&*document)
                    .and_then(|ids| ids.get(syntax_id.as_str()))
                    .copied();
                let kept = match known {
                    Some(kept) => kept,
                    None => {
                        // A captured exact definition is a named captured declaration (its
                        // exact declaration-name join anchors on the name). An unshared pin
                        // keeps captured status only for the other targets; a shared pin's
                        // captured status covers every target, so it needs no probe.
                        let original = (self.destination.rev != self.captured.rev
                            && self.captured_definition(document, syntax_id)?)
                            || self
                                .declaration_status(self.captured.rev, document, syntax_id)?
                                .iter()
                                .any(|status| status.0);
                        let kept = original && {
                            let candidates: Vec<_> = self
                                .declaration_status(self.destination.rev, document, syntax_id)?
                                .into_iter()
                                .filter(|status| status.0)
                                .collect();
                            candidates.len() == 1
                                && candidates[0].1
                                && self.destination.doc_row(document)?.is_some()
                        };
                        // The memo is optional: without room for an entry it is skipped.
                        let document_entry = if memo.internal.contains_key(&*document) {
                            0
                        } else {
                            sizes::sum(&[
                                sizes::map_entries::<native::DocumentKey, BTreeMap<String, bool>>(
                                    1,
                                )?,
                                sizes::document(document)?,
                            ])?
                        };
                        let bytes = sizes::sum(&[
                            document_entry,
                            sizes::map_entries::<String, bool>(1)?,
                            syntax_id.len() as u64,
                        ])?;
                        if memo.retained.grow(bytes).is_ok() {
                            memo.internal
                                .entry(document.clone())
                                .or_default()
                                .insert(syntax_id.clone(), kept);
                        }
                        kept
                    }
                };
                if kept && revision_id != self.destination_id() {
                    owner.grow(self.destination_id().len() as u64)?;
                    *revision_id = self.destination_id().to_owned();
                }
                Ok(kept)
            }
        }
    }

    /// `retarget` over every target in order, stopping at the first that fails.
    fn retarget_all<'t>(
        &self,
        targets: impl IntoIterator<Item = &'t mut semantic::Target>,
        memo: &mut Retargets,
        owner: &mut Reservation,
    ) -> Result<bool> {
        for target in targets {
            if !self.retarget(target, memo, owner)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn id_for(&self, raw: &str, kind: semantic::EvidenceKind) -> String {
        rejoin_provenance_id(raw, self.destination_id(), kind)
    }

    /// Runs the emission validator's row-local checks for the row stemming from raw proof
    /// `raw` of `view`'s document, against that document's destination group. `id` is the
    /// row's provenance ID as derived from `raw` (`id_for`), before any test edit of the row.
    /// `anchor` is the document the row itself names. `index` holds the group's validation
    /// index, built and charged on the document's first row and released with the document.
    #[allow(clippy::too_many_arguments)]
    fn check_row(
        &self,
        view: &StagedSide<'_>,
        index: &ValidationIndex,
        raw: &str,
        id: &str,
        kind: semantic::EvidenceKind,
        anchor: Option<&native::DocumentKey>,
        check: impl for<'a> FnOnce(
            &'a EmissionValidator<'_>,
            &semantic::Provenance,
            &semantic::DocumentFacts<'a>,
        ) -> Result<()>,
    ) -> Result<()> {
        view.proof(raw)?
            .context("dangling semantic fact provenance")?;
        let group = view
            .local_group
            .as_ref()
            .context("missing destination document")?;
        let document = &group.document.key;
        anyhow::ensure!(
            anchor.is_none_or(|anchor| anchor == document),
            "semantic fact/provenance tuple mismatch"
        );
        let proof =
            self.validator
                .fact_proof(id, document.clone(), kind, &group.document.content_hash);
        let source = group.text.as_bytes();
        let sources = |candidate: &native::DocumentKey| (candidate == document).then_some(source);
        let declaration = |id: &str| {
            group
                .declaration_ids
                .get(id)
                .and_then(|found| found.first())
                .map(|index| &group.native.declarations[*index])
        };
        let reference = |id: &str| {
            group
                .reference_ids
                .get(id)
                .and_then(|found| found.first())
                .map(|index| &group.native.references[*index])
        };
        let known = |id: &str, document: &native::DocumentKey| -> Result<bool> {
            Ok(!self
                .declaration_status(self.destination.rev, document, id)?
                .is_empty())
        };
        let index = validation_index(index, &self.ctx.live, &group.native)?;
        check(
            &self.validator,
            &proof,
            &semantic::DocumentFacts {
                native: &group.native,
                index,
                sources: &sources,
                declaration: &declaration,
                reference: &reference,
                known: &known,
            },
        )
    }

    /// Settles one eligible document and writes its validated rows. The document's captured
    /// and destination joins share one decoded claim set and are charged to the same live
    /// budget as every owner built here; all of it is released before the next document.
    pub(super) fn settle_document(
        &self,
        doc: &native::DocumentKey,
        captured_global: &CompletedGlobal<'_>,
        destination_global: &CompletedGlobal<'_>,
        destination_manifest_hash: &str,
        prepared: Option<super::CapturedJoin>,
    ) -> Result<DocumentOutcome> {
        let encoding = self.admitted_encoding;
        let _keys = self.view_keys(doc, 2)?;
        // The captured join eligibility already computed for this document, over the same
        // claims and captured context, or a fresh one.
        let (claims, mut captured, _captured_reservation) = match prepared {
            Some(joined) => (joined.claims, joined.facts, joined.reservation),
            None => {
                let claims = Rc::new(DocumentClaims::load(&self.filtered, doc)?);
                let captured_view =
                    streaming::claims_side(&self.captured, doc.clone(), claims.clone())?;
                let (facts, reservation) =
                    streaming::join_document(&captured_view, captured_global, encoding)?;
                (claims, facts, reservation)
            }
        };
        let view = streaming::claims_side(&self.filtered, doc.clone(), claims)?;
        // The document's validation index, dropped (and its charge released) with the document.
        let index = ValidationIndex::new();
        // Same claims, native revision and global rows: the destination join would recompute
        // the captured facts, so the destination rows are read from the captured join itself.
        // Settlement reads destination rows by reference and copies one at a time.
        let (separate, _destination_reservation) = if self.identical_joins.get() {
            (None, None)
        } else {
            let (facts, reservation) =
                streaming::join_document(&view, destination_global, encoding)?;
            (Some(facts), Some(reservation))
        };
        #[cfg(feature = "test-support")]
        crate::semantic_batch::testing::run_document_common_observer(
            1,
            doc,
            separate.as_ref().unwrap_or(&captured),
            self.conn(),
        );
        let relationships = std::mem::take(&mut captured.type_relationships);
        let captured = &captured;
        let destination = separate.as_ref().unwrap_or(captured);
        let live = &self.ctx.live;
        let mut owner = self.ctx.live.reserve(0)?;
        let mut memo = Retargets {
            internal: BTreeMap::new(),
            external: BTreeMap::new(),
            retained: self.ctx.live.reserve(0)?,
        };
        let mut outcome = DocumentOutcome {
            diagnostics: captured.diagnostics.contains_key(doc),
            ..DocumentOutcome::default()
        };
        // Raw proofs that published a fact, by evidence kind, and every claim's final outcome.
        let mut sources = BTreeSet::<(String, semantic::EvidenceKind)>::new();
        let mut outcomes = BTreeMap::<String, i64>::new();
        let source = |sources: &mut BTreeSet<(String, semantic::EvidenceKind)>,
                      owner: &mut Reservation,
                      raw: &str,
                      kind: semantic::EvidenceKind| {
            if !sources.contains(&(raw.to_owned(), kind)) {
                entry::<(String, semantic::EvidenceKind), ()>(owner, raw.len() as u64)?;
                sources.insert((raw.to_owned(), kind));
            }
            anyhow::Ok(())
        };
        let set_outcome = |outcomes: &mut BTreeMap<String, i64>,
                           owner: &mut Reservation,
                           raw: &str,
                           value: i64| {
            if let Some(slot) = outcomes.get_mut(raw) {
                *slot = value;
            } else {
                entry::<String, i64>(owner, raw.len() as u64)?;
                outcomes.insert(raw.to_owned(), value);
            }
            anyhow::Ok(())
        };

        // Captured exact definitions of this document, by symbol, and by authoring proof.
        let mut captured_targets = BTreeMap::<&str, Vec<semantic::Target>>::new();
        let mut captured_declarations = BTreeMap::<&str, &str>::new();
        for binding in &captured.declaration_bindings {
            let Some(syntax_id) = &binding.syntax_id else {
                continue;
            };
            entry::<&str, &str>(&mut owner, 0)?;
            captured_declarations.insert(binding.provenance_id.as_str(), syntax_id.as_str());
            if &binding.join.anchor.document != doc {
                continue;
            }
            for symbol in &binding.symbols {
                // Admitted from borrowed parts, built only after admission. The map entry is
                // charged with every push: a conservative upper bound.
                let payload = sizes::sum(&[
                    sizes::map_entries::<&str, Vec<semantic::Target>>(1)?,
                    syntax_id.len() as u64,
                    sizes::document(doc)?,
                    self.captured_id().len() as u64,
                ])?;
                let targets = captured_targets.entry(symbol.symbol.as_str()).or_default();
                streaming::append_reserved_with(targets, payload, &mut owner, || {
                    semantic::Target::Internal {
                        syntax_id: syntax_id.clone(),
                        document: doc.clone(),
                        revision_id: self.captured_id().to_owned(),
                    }
                })?;
            }
        }
        for targets in captured_targets.values_mut() {
            canonical_targets(&self.ctx.live, targets)?;
        }

        for row in &destination.symbols {
            self.ctx.check_cancel()?;
            let (mut item, mut symbol) = working_copy(live, row, sizes::symbol)?;
            let own = view.proof(&symbol.provenance_id)?.is_some();
            if own && let Some(targets) = captured_targets.get(symbol.key.symbol.as_str()) {
                let replaced = sizes::targets(&symbol.declarations)?;
                item.grow(sizes::targets(targets)?)?;
                symbol.declarations = targets.clone();
                item.release(replaced)?;
            }
            if self.retarget_all(&mut symbol.declarations, &mut memo, &mut item)? {
                let raw = symbol.provenance_id.clone();
                let kind = semantic::EvidenceKind::DeclarationBinding;
                source(&mut sources, &mut owner, &raw, kind)?;
                let id = self.id_for(&raw, kind);
                symbol.provenance_id = id.clone();
                #[cfg(feature = "test-support")]
                crate::semantic_batch::testing::edit_row("symbol", &mut symbol)?;
                self.check_row(&view, &index, &raw, &id, kind, None, |v, p, f| {
                    v.symbol(&symbol, p, f)
                })?;
                self.emit_fact("out_symbols", SYMBOLS, &mut symbol)?;
            } else if own {
                outcome.omitted = true;
            }
        }

        for row in &destination.declaration_bindings {
            let Some(syntax_id) = &row.syntax_id else {
                continue;
            };
            if captured_declarations.get(row.provenance_id.as_str()) != Some(&syntax_id.as_str()) {
                continue;
            }
            let (_item, mut binding) = working_copy(live, row, sizes::binding)?;
            let raw = binding.provenance_id.clone();
            let kind = semantic::EvidenceKind::DeclarationBinding;
            source(&mut sources, &mut owner, &raw, kind)?;
            set_outcome(&mut outcomes, &mut owner, &raw, 1)?;
            outcome.observed.insert(semantic::Role::Definition);
            let id = self.id_for(&raw, kind);
            binding.provenance_id = id.clone();
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("declarationBinding", &mut binding)?;
            self.check_row(
                &view,
                &index,
                &raw,
                &id,
                kind,
                Some(&binding.join.anchor.document),
                |v, p, f| v.declaration_binding(&binding, p, f),
            )?;
            self.emit_fact("out_declaration_bindings", BINDINGS, &mut binding)?;
        }

        // Each destination reference claim takes its captured reference's resolution; claims
        // of one native reference merge into one row over the union of their targets. A row is
        // kept as the destination claim it starts from plus its merged roles and site; the
        // emitted row is rebuilt from them, and its targets are the union's.
        let mut rows = BTreeMap::<String, PendingReference<'_>>::new();
        let mut targets = BTreeMap::<String, Vec<semantic::Target>>::new();
        let mut proofs = BTreeMap::<String, Vec<String>>::new();
        let mut accepted = BTreeMap::<String, String>::new();
        for (raw, row) in &destination.reference_claims {
            let Some(prior) = captured.reference_claims.get(raw) else {
                continue;
            };
            anyhow::ensure!(
                prior.document == row.document && prior.range == row.range,
                "captured reference anchor differs"
            );
            if !row.roles.iter().any(|role| prior.roles.contains(role)) {
                continue;
            }
            let (mut item, mut reference) = working_copy(live, row, sizes::reference)?;
            reference.roles.retain(|role| prior.roles.contains(role));
            reference.resolution = prior.resolution;
            adopt_targets(
                &mut item,
                &mut reference.declared_target,
                &mut reference.candidates,
                prior.declared_target.as_ref(),
                &prior.candidates,
            )?;
            if !self.retarget_all(
                reference
                    .declared_target
                    .iter_mut()
                    .chain(reference.candidates.iter_mut()),
                &mut memo,
                &mut item,
            )? {
                outcome.omitted = true;
                continue;
            }
            let id = reference.id.clone();
            if !targets.contains_key(&id) {
                entry::<String, Vec<semantic::Target>>(&mut owner, id.len() as u64)?;
                entry::<String, Vec<String>>(&mut owner, id.len() as u64)?;
            }
            let united = targets.entry(id.clone()).or_default();
            for target in reference
                .declared_target
                .iter()
                .chain(&reference.candidates)
            {
                streaming::append_reserved_with(
                    united,
                    sizes::target_payload(target)?,
                    &mut owner,
                    || target.clone(),
                )?;
            }
            streaming::append_reserved_with(
                proofs.entry(id.clone()).or_default(),
                raw.len() as u64,
                &mut owner,
                || raw.clone(),
            )?;
            entry::<String, String>(&mut owner, (raw.len() + id.len()) as u64)?;
            accepted.insert(raw.clone(), id.clone());
            source(
                &mut sources,
                &mut owner,
                raw,
                semantic::EvidenceKind::SemanticReference,
            )?;
            if let Some(pending) = rows.get_mut(&id) {
                let roles: BTreeSet<_> = pending
                    .roles
                    .iter()
                    .chain(&reference.roles)
                    .copied()
                    .collect();
                owner.grow((roles.len() * std::mem::size_of::<semantic::Role>()) as u64)?;
                pending.roles = roles.into_iter().collect();
                if pending.roles.contains(&semantic::Role::Definition) {
                    pending.site = Some(semantic::ReferenceSite::Declaration);
                }
            } else {
                entry::<String, PendingReference<'_>>(
                    &mut owner,
                    sizes::sum(&[
                        id.len() as u64,
                        (reference.roles.capacity() * std::mem::size_of::<semantic::Role>()) as u64,
                    ])?,
                )?;
                rows.insert(
                    id,
                    PendingReference {
                        claim: row,
                        roles: std::mem::take(&mut reference.roles),
                        site: None,
                    },
                );
            }
        }
        // Final references in reference-ID order; the emitted row's targets are what calls copy.
        let mut emitted = BTreeMap::<String, EmittedTargets>::new();
        for (id, pending) in rows {
            let mut united = targets.remove(&id).unwrap_or_default();
            canonical_targets(&self.ctx.live, &mut united)?;
            // The row is its first claim's destination row with the merged roles and site; its
            // own targets give way to the union (which `owner` already holds).
            let (mut item, mut reference) = working_copy(live, pending.claim, sizes::reference)?;
            let replaced = target_set(reference.declared_target.as_ref(), &reference.candidates)?;
            reference.declared_target = None;
            reference.candidates = Vec::new();
            item.release(replaced)?;
            reference.roles = pending.roles;
            if let Some(site) = pending.site {
                reference.site = site;
            }
            if united.len() > 1 {
                reference.resolution = semantic::Resolution::Ambiguous;
                reference.candidates = united;
            } else if let Some(target) = united.pop() {
                reference.resolution = if matches!(target, semantic::Target::Internal { .. }) {
                    semantic::Resolution::Resolved
                } else {
                    semantic::Resolution::External
                };
                reference.declared_target = Some(target);
            } else {
                reference.resolution = semantic::Resolution::Unresolved;
            }
            let value = if reference.resolution == semantic::Resolution::Ambiguous {
                3
            } else {
                1
            };
            set_outcome(&mut outcomes, &mut owner, &reference.provenance_id, value)?;
            for raw in proofs.remove(&id).unwrap_or_default() {
                set_outcome(&mut outcomes, &mut owner, &raw, value)?;
            }
            outcome.observed.extend(reference.roles.iter().copied());
            let raw = reference.provenance_id.clone();
            let kind = semantic::EvidenceKind::SemanticReference;
            let proof_id = self.id_for(&raw, kind);
            reference.provenance_id = proof_id.clone();
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("reference", &mut reference)?;
            self.check_row(
                &view,
                &index,
                &raw,
                &proof_id,
                kind,
                Some(&reference.document),
                |v, p, f| v.reference(&reference, p, f),
            )?;
            let body = codec::fact_body(&mut reference)?;
            self.emit(
                "out_references",
                "INSERT INTO out_references VALUES(?1,?2,?3,?4,?5)",
                &[
                    &self.next_ord(REFERENCES),
                    &self.current_document(),
                    &reference.id,
                    &codec::digest(&reference.provenance_id),
                    &body,
                ],
            )?;
            // Calls read the emitted targets only; the rest of the row drops with this item.
            entry::<String, EmittedTargets>(&mut owner, id.len() as u64)?;
            emitted.insert(
                id,
                EmittedTargets {
                    resolution: reference.resolution,
                    declared_target: reference.declared_target.take(),
                    candidates: std::mem::take(&mut reference.candidates),
                },
            );
        }

        // Calls take the captured reference's targets, then their final reference's.
        // Each also keeps its captured binding, whose join-time dispatch (#470) the rejoin may
        // settle on.
        let mut old_targets =
            BTreeMap::<&str, (&semantic::Reference, &semantic::CallBinding)>::new();
        for binding in &captured.call_bindings {
            if let Some(reference) = captured.reference_claims.get(&binding.provenance_id) {
                entry::<&str, (&semantic::Reference, &semantic::CallBinding)>(&mut owner, 0)?;
                old_targets.insert(binding.provenance_id.as_str(), (reference, binding));
            }
        }
        let mut calls = Vec::new();
        for row in &destination.call_bindings {
            let Some(&(old, captured_call)) = old_targets.get(row.provenance_id.as_str()) else {
                continue;
            };
            // The working copy, then the captured reference's targets it adopts.
            let (mut item, mut call) = working_copy(live, row, sizes::call)?;
            call.resolution = old.resolution;
            adopt_targets(
                &mut item,
                &mut call.declared_target,
                &mut call.candidates,
                old.declared_target.as_ref(),
                &old.candidates,
            )?;
            if !self.retarget_all(
                call.declared_target
                    .iter_mut()
                    .chain(call.candidates.iter_mut()),
                &mut memo,
                &mut item,
            )? {
                outcome.omitted = true;
                continue;
            }
            source(
                &mut sources,
                &mut owner,
                &call.provenance_id,
                semantic::EvidenceKind::SemanticReference,
            )?;
            let Some(reference) = accepted
                .get(&call.provenance_id)
                .and_then(|id| emitted.get(id))
            else {
                continue;
            };
            call.resolution = reference.resolution;
            adopt_targets(
                &mut item,
                &mut call.declared_target,
                &mut call.candidates,
                reference.declared_target.as_ref(),
                &reference.candidates,
            )?;
            call.stale_target = call.declared_target.as_ref().and_then(|target| {
                matches!(target, semantic::Target::Internal { .. }).then_some(false)
            });
            // `row` is the destination join's row, classified at join time.
            call.dispatch = super::super::rejoined_dispatch(
                Some((row.dispatch, row.declared_target.as_ref())),
                Some((
                    captured_call.dispatch,
                    captured_call.declared_target.as_ref(),
                )),
                &call,
                |syntax_id, document| {
                    Ok(self
                        .declaration_status(self.destination.rev, document, syntax_id)?
                        .iter()
                        .any(|status| status.2))
                },
            )?;
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("callResolution", &mut call)?;
            // The kept copy's payload moves to the owner; its root moves into a slot of
            // `calls`, which `append_reserved` charges with the buffer.
            let payload = item.bytes() - std::mem::size_of::<semantic::CallBinding>() as u64;
            item.transfer_to(&mut owner, payload)?;
            streaming::append_reserved(&mut calls, call, 0, &mut owner)?;
        }
        // Every binding of one native call site carries the site's whole distinct target set:
        // contradictory exact evidence becomes ambiguous over that union, never first-wins.
        let mut sites = BTreeMap::<String, BTreeMap<Vec<u8>, semantic::Target>>::new();
        for call in &calls {
            let Some(site) = &call.call_id else {
                continue;
            };
            if !sites.contains_key(site) {
                entry::<String, BTreeMap<Vec<u8>, semantic::Target>>(
                    &mut owner,
                    site.len() as u64,
                )?;
                sites.insert(site.clone(), BTreeMap::new());
            }
            for target in call.declared_target.iter().chain(&call.candidates) {
                let canonical = canonical_target(target)?;
                let union = sites.get_mut(site).context("missing call site entry")?;
                if let std::collections::btree_map::Entry::Vacant(slot) = union.entry(canonical) {
                    entry::<Vec<u8>, semantic::Target>(
                        &mut owner,
                        sizes::sum(&[slot.key().len() as u64, sizes::target_payload(target)?])?,
                    )?;
                    slot.insert(target.clone());
                }
            }
        }
        let mut unions = BTreeMap::<String, Vec<semantic::Target>>::new();
        for (site, union) in sites {
            if union.len() > 1 {
                // The values move; only the Vec buffer and the map entry are new.
                owner.grow(sizes::sum(&[
                    sizes::map_entries::<String, Vec<semantic::Target>>(1)?,
                    target_size::target_slots(union.len())?,
                ])?)?;
                unions.insert(site, union.into_values().collect());
            }
        }
        for call in &mut calls {
            let Some(union) = call.call_id.as_deref().and_then(|site| unions.get(site)) else {
                continue;
            };
            if call.resolution == semantic::Resolution::Ambiguous
                && call.declared_target.is_none()
                && call.candidates == *union
            {
                continue;
            }
            let replaced = target_set(call.declared_target.as_ref(), &call.candidates)?;
            owner.grow(sizes::targets(union)?)?;
            call.resolution = semantic::Resolution::Ambiguous;
            call.declared_target = None;
            call.candidates = union.clone();
            call.stale_target = None;
            call.dispatch = semantic::Dispatch::Unknown;
            owner.release(replaced)?;
        }
        for mut call in calls {
            // A call's outcome follows its final row, after the per-document reference
            // outcomes. It differs from `finish_rejoin`'s precedence only where call-site
            // grouping made the call ambiguous over a resolved reference: the old staged rule,
            // since the whole-corpus oracle does not group call sites.
            let raw = call.provenance_id.clone();
            let value = if call.resolution == semantic::Resolution::Ambiguous {
                3
            } else {
                1
            };
            set_outcome(&mut outcomes, &mut owner, &raw, value)?;
            outcome.observed.insert(semantic::Role::Call);
            let kind = semantic::EvidenceKind::SemanticReference;
            let id = self.id_for(&raw, kind);
            call.provenance_id = id.clone();
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("callBinding", &mut call)?;
            self.check_row(
                &view,
                &index,
                &raw,
                &id,
                kind,
                Some(&call.join.anchor.document),
                |v, p, f| v.call_binding(&call, p, f),
            )?;
            self.emit_fact("out_call_bindings", CALLS, &mut call)?;
        }

        // Captured type relationships of eligible claims survive when both endpoints retarget.
        for mut relationship in relationships {
            if view.proof(&relationship.provenance_id)?.is_none() {
                continue;
            }
            if self.retarget(&mut relationship.source, &mut memo, &mut owner)?
                && self.retarget(&mut relationship.target, &mut memo, &mut owner)?
            {
                let raw = relationship.provenance_id.clone();
                let kind = semantic::EvidenceKind::TypeRelationship;
                source(&mut sources, &mut owner, &raw, kind)?;
                set_outcome(&mut outcomes, &mut owner, &raw, 1)?;
                outcome.observed.insert(semantic::Role::Type);
                let id = self.id_for(&raw, kind);
                relationship.provenance_id = id.clone();
                #[cfg(feature = "test-support")]
                crate::semantic_batch::testing::edit_row("typeRelationship", &mut relationship)?;
                self.check_row(&view, &index, &raw, &id, kind, None, |v, p, f| {
                    v.type_relationship(&relationship, p, f)
                })?;
                self.emit_fact("out_type_relationships", RELATIONSHIPS, &mut relationship)?;
            } else {
                outcome.omitted = true;
            }
        }

        outcome.provenance = self.provenance(&view, doc, &sources, destination_manifest_hash)?;
        for value in outcomes.values() {
            match value {
                1 => outcome.matched += 1,
                3 => outcome.ambiguous += 1,
                _ => {}
            }
        }
        Ok(outcome)
    }

    /// Writes one validated fact row of the current document.
    fn emit_fact<T: codec::ProvenanceOwned>(
        &self,
        table: &'static str,
        family: usize,
        row: &mut T,
    ) -> Result<()> {
        let provenance = codec::digest(row.provenance_id());
        let body = codec::fact_body(row)?;
        self.emit(
            table,
            &format!("INSERT INTO {table} VALUES(?1,?2,?3,?4)"),
            &[
                &self.next_ord(family),
                &self.current_document(),
                &provenance,
                &body,
            ],
        )
    }

    /// Writes this document's provenance in (raw ID, evidence kind) order and returns the
    /// stored template its rows share, for the document's coverage row.
    fn provenance(
        &self,
        view: &StagedSide<'_>,
        doc: &native::DocumentKey,
        sources: &BTreeSet<(String, semantic::EvidenceKind)>,
        destination_manifest_hash: &str,
    ) -> Result<Option<String>> {
        if sources.is_empty() {
            return Ok(None);
        }
        // Every derived proof is a claim of this document, decoded from its staged row.
        let lookup = |proof: &str, kind: &str, digest: &str| -> Result<RawClaimDocument> {
            let (claim, _) = view.local_claim(proof, kind, digest)?;
            Ok(RawClaimDocument {
                language: claim.language.clone(),
                path: claim.envelope.relative_path.clone(),
                artifact_hash: claim.artifact_hash.clone(),
            })
        };
        let basis = self
            .basis
            .basis_for(doc)?
            .context("missing immutable captured basis")?;
        let (_, destination) = self
            .destination
            .doc_row(doc)?
            .context("missing destination document")?;
        let captured_hash = self.captured.doc_row(doc)?.map(|(_, d)| d.content_hash);
        let header = &self.destination_header.revision;
        let freshness = if captured_hash.is_none_or(|hash| hash != destination.content_hash) {
            semantic::Freshness::Stale
        } else if basis.revision_id != header.id
            || basis.source_manifest_hash != destination_manifest_hash
            || basis.toolchain_hash != header.toolchain_hash
            || basis.config_hash != header.config_hash
            || basis.dependency_hash != header.dependency_hash
            || basis.producer_version != self.producer.version
            || basis.producer_hash != self.producer.executable_hash
            || !self.producer.languages.contains(&basis.language)
        {
            semantic::Freshness::PossiblyStale
        } else {
            semantic::Freshness::Fresh
        };
        let mut template = None;
        for (raw, kind) in sources {
            self.ctx.check_cancel()?;
            let claim = view
                .proof(raw)?
                .context("missing raw fact for joined provenance")?;
            view.check_proof(raw, &claim.record_kind, &claim.fact_digest)?;
            anyhow::ensure!(
                claim.language == doc.language && claim.path == doc.path,
                "joined provenance outside its document"
            );
            anyhow::ensure!(
                basis.revision_id == self.captured_id()
                    && basis.artifact_hash == claim.artifact_hash
                    && basis.source_manifest_hash == self.descriptor.source_manifest_hash
                    && basis.producer_id == self.producer.id
                    && basis.language == language_of(&doc.language)?,
                "captured semantic basis differs from authenticated inventory"
            );
            let derived_kind: semantic::RecordKind =
                serde_json::from_value(serde_json::json!(claim.record_kind))?;
            let provenance = semantic::Provenance {
                id: self.id_for(raw, *kind),
                producer_id: self.producer.id.clone(),
                document: doc.clone(),
                revision_id: header.id.clone(),
                content_hash: destination.content_hash.clone(),
                evidence_kind: *kind,
                basis: Some(basis.clone()),
                freshness,
                derived_from: Some(semantic::DerivedFrom {
                    provenance_id: raw.clone(),
                    record_kind: derived_kind,
                    fact_digest: claim.fact_digest.clone(),
                }),
            };
            self.validator.provenance(&provenance, &lookup)?;
            if template.is_none() {
                template = Some(codec::provenance_template(&provenance)?);
            }
            self.ctx.charge(1)?;
            let (_, claim_id) = view.local_claim(raw, &claim.record_kind, &claim.fact_digest)?;
            self.emit(
                "out_provenance",
                "INSERT INTO out_provenance VALUES(?1,?2,?3,?4,?5)",
                &[
                    &self.next_ord(PROVENANCE),
                    &self.current_document(),
                    &claim_id,
                    &codec::kind_rank(*kind),
                    &codec::digest(&provenance.id),
                ],
            )?;
        }
        Ok(template)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_index_is_admitted_before_it_is_built_and_released_with_its_document() {
        let native = native::Artifact {
            producer: native::Producer {
                id: "native".into(),
                version: "1".into(),
                executable_hash: "e".repeat(64),
                kind: "native".into(),
                languages: vec!["rust".into()],
                position_encoding: "utf8".into(),
            },
            source_set: native::SourceSet {
                id: "set".into(),
                root_id: "root".into(),
                languages: vec!["rust".into()],
                dependencies: vec![],
            },
            revision: native::Revision {
                id: "revision".into(),
                source_set_id: "set".into(),
                // A nonempty document, so building the index allocates.
                documents: vec![native::Document {
                    key: native::DocumentKey {
                        source_set_id: "set".into(),
                        language: "rust".into(),
                        path: "src/lib.rs".into(),
                    },
                    revision_id: "revision".into(),
                    content_hash: "c".repeat(64),
                    byte_length: 0,
                }],
                toolchain_hash: "t".into(),
                config_hash: "c".into(),
                dependency_hash: "d".into(),
            },
            coverage: vec![],
            provenance: vec![],
            declarations: vec![],
            calls: vec![],
            control_regions: vec![],
            references: vec![],
            capability_reports: vec![],
            search_cards: vec![],
        };
        let bytes = semantic::NativeIndex::bytes(&native);
        assert!(bytes > 0);
        // No room: typed refusal, and nothing allocated before it, not even the estimate's
        // scratch: the refusal peaks exactly where the budget's own refusal does.
        let budget = LiveBudget::new(bytes - 1);
        let cell = ValidationIndex::new();
        let (refused, _, refusal_peak) =
            crate::test_alloc::measure_peak(|| validation_index(&cell, &budget, &native).err());
        let (_, _, reserve_peak) = crate::test_alloc::measure_peak(|| budget.reserve(bytes).err());
        assert_eq!(refusal_peak, reserve_peak);
        let error = refused.unwrap();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(), Some(ScipCaptureError::LimitExceeded(name)) if name == "unit expansion")
        );
        assert!(cell.get().is_none());
        // Room: built once and held by the document's cell, then released with it.
        let budget = LiveBudget::new(bytes);
        let cell = ValidationIndex::new();
        validation_index(&cell, &budget, &native).unwrap();
        validation_index(&cell, &budget, &native).unwrap();
        assert_eq!(budget.high_water(), bytes);
        assert!(budget.reserve(1).is_err());
        drop(cell);
        budget.reserve(bytes).unwrap();
    }

    #[test]
    fn adopted_targets_are_admitted_from_the_incoming_set_before_any_copy() {
        let document = |path: &str| native::DocumentKey {
            source_set_id: "source".into(),
            language: "javascript".into(),
            path: path.into(),
        };
        // Destination discovery excluded the ineligible B.js, so the destination call holds
        // a small external target; the captured reference names B.js's declaration.
        let mut declared = Some(semantic::Target::External {
            symbol: super::super::super::symbol_key("pkg target.", &document("src/A.js")),
        });
        let mut candidates = Vec::new();
        let captured = semantic::Target::Internal {
            syntax_id: "sid:v1:".repeat(8),
            document: document("src/B.js"),
            revision_id: "revision:v2:".repeat(8),
        };
        let others = vec![captured.clone(), captured.clone()];
        let incoming = sizes::target_payload(&captured).unwrap() + sizes::targets(&others).unwrap();
        let existing = sizes::target_payload(declared.as_ref().unwrap()).unwrap();
        assert!(incoming > existing, "the incoming set is the larger one");

        // The owner holds the row's current set. Room for a copy of the replaced set, not of
        // the incoming one: denied, nothing copied.
        let budget = LiveBudget::new(existing + incoming - 1);
        let mut owner = budget.reserve(existing).unwrap();
        let before = declared.clone();
        let error = adopt_targets(
            &mut owner,
            &mut declared,
            &mut candidates,
            Some(&captured),
            &others,
        )
        .unwrap_err();
        assert!(
            matches!(error.downcast_ref::<ScipCaptureError>(),Some(ScipCaptureError::LimitExceeded(name)) if name=="unit expansion")
        );
        assert_eq!(declared, before);
        assert!(candidates.is_empty());
        assert_eq!(
            budget.high_water(),
            existing,
            "nothing was admitted or copied"
        );

        let budget = LiveBudget::new(existing + incoming);
        let mut owner = budget.reserve(existing).unwrap();
        adopt_targets(
            &mut owner,
            &mut declared,
            &mut candidates,
            Some(&captured),
            &others,
        )
        .unwrap();
        assert_eq!(declared.as_ref(), Some(&captured));
        assert_eq!(candidates, others);
        // Both sets were live at the swap; afterwards the owner holds the adopted one only.
        assert_eq!(budget.high_water(), existing + incoming);
        assert_eq!(owner.bytes(), incoming);
    }
}
