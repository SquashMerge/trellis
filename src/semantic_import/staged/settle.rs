//! Pass 4 of the streaming join: one eligible document's rejoin settlement. Its captured and
//! eligible-destination joins are recomputed from the claim store and the two completed global
//! contexts, settled by the rules of the whole-corpus rejoin (`finish_rejoin`) restricted to
//! that document, validated row by row and written to `out_*`. Every rule that settlement reads
//! is document-local: a fact's raw proof, its reference and call site all belong to the
//! document; only target authority (`nat_declarations`, captured exact definitions) is global.
use super::streaming::{self, CompletedGlobal};
use super::*;

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

/// Replaces `declared`/`candidates` with copies of `from_declared`/`from_candidates`. The
/// copies are admitted from the borrowed source before anything is cloned; the replaced values
/// stay charged (overlap). The incoming set can be larger than the one it replaces: a captured
/// internal target may survive retargeting although destination discovery excluded its
/// ineligible document.
fn adopt_targets(
    owner: &mut Reservation,
    declared: &mut Option<semantic::Target>,
    candidates: &mut Vec<semantic::Target>,
    from_declared: Option<&semantic::Target>,
    from_candidates: &[semantic::Target],
) -> Result<()> {
    owner.grow(sizes::sum(&[
        from_declared.map_or(Ok(0), sizes::target_payload)?,
        sizes::targets(from_candidates)?,
    ])?)?;
    *declared = from_declared.cloned();
    *candidates = from_candidates.to_vec();
    Ok(())
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
                        let original = self
                            .declaration_status(self.captured.rev, document, syntax_id)?
                            .iter()
                            .any(|(named, _)| *named);
                        let kept = original && {
                            let candidates: Vec<_> = self
                                .declaration_status(self.destination.rev, document, syntax_id)?
                                .into_iter()
                                .filter(|(named, _)| *named)
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
    /// `raw` of `view`'s document, against that document's destination group. `anchor` is the
    /// document the row itself names.
    fn check_row(
        &self,
        view: &StagedSide<'_>,
        raw: &str,
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
        let proof = self.validator.fact_proof(
            &self.id_for(raw, kind),
            document.clone(),
            kind,
            &group.document.content_hash,
        );
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
        check(
            &self.validator,
            &proof,
            &semantic::DocumentFacts {
                native: &group.native,
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
    ) -> Result<DocumentOutcome> {
        let encoding = self.admitted_encoding;
        let claims = Rc::new(DocumentClaims::load(&self.filtered, doc)?);
        let _keys = self.view_keys(doc, 2)?;
        let captured_view = streaming::claims_side(&self.captured, doc.clone(), claims.clone())?;
        let view = streaming::claims_side(&self.filtered, doc.clone(), claims)?;
        let (mut captured, _captured_reservation) =
            streaming::join_document(&captured_view, captured_global, encoding)?;
        let (destination, _destination_reservation) =
            streaming::join_document(&view, destination_global, encoding)?;
        #[cfg(feature = "test-support")]
        crate::semantic_batch::testing::run_document_common_observer(
            1,
            doc,
            &destination,
            self.conn(),
        );
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
        let relationships = std::mem::take(&mut captured.type_relationships);

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

        for mut symbol in destination.symbols {
            self.ctx.check_cancel()?;
            let own = view.proof(&symbol.provenance_id)?.is_some();
            if own && let Some(targets) = captured_targets.get(symbol.key.symbol.as_str()) {
                owner.grow(sizes::targets(targets)?)?;
                symbol.declarations = targets.clone();
            }
            if self.retarget_all(&mut symbol.declarations, &mut memo, &mut owner)? {
                let raw = symbol.provenance_id.clone();
                let kind = semantic::EvidenceKind::DeclarationBinding;
                source(&mut sources, &mut owner, &raw, kind)?;
                symbol.provenance_id = self.id_for(&raw, kind);
                #[cfg(feature = "test-support")]
                crate::semantic_batch::testing::edit_row("symbol", &mut symbol)?;
                self.check_row(&view, &raw, kind, None, |v, p, f| v.symbol(&symbol, p, f))?;
                let body = to_json(&symbol)?;
                self.emit(
                    "out_symbols",
                    "INSERT INTO out_symbols VALUES(?1,?2,?3)",
                    &[&self.next_ord(SYMBOLS), &symbol.provenance_id, &body],
                )?;
            } else if own {
                outcome.omitted = true;
            }
        }

        for mut binding in destination.declaration_bindings {
            let Some(syntax_id) = &binding.syntax_id else {
                continue;
            };
            if captured_declarations.get(binding.provenance_id.as_str())
                != Some(&syntax_id.as_str())
            {
                continue;
            }
            let raw = binding.provenance_id.clone();
            let kind = semantic::EvidenceKind::DeclarationBinding;
            source(&mut sources, &mut owner, &raw, kind)?;
            set_outcome(&mut outcomes, &mut owner, &raw, 1)?;
            outcome.observed.insert(semantic::Role::Definition);
            binding.provenance_id = self.id_for(&raw, kind);
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("declarationBinding", &mut binding)?;
            self.check_row(
                &view,
                &raw,
                kind,
                Some(&binding.join.anchor.document),
                |v, p, f| v.declaration_binding(&binding, p, f),
            )?;
            let body = to_json(&binding)?;
            self.emit(
                "out_declaration_bindings",
                "INSERT INTO out_declaration_bindings VALUES(?1,?2,?3)",
                &[&self.next_ord(BINDINGS), &binding.provenance_id, &body],
            )?;
        }

        // Each destination reference claim takes its captured reference's resolution; claims
        // of one native reference merge into one row over the union of their targets.
        let mut rows = BTreeMap::<String, semantic::Reference>::new();
        let mut targets = BTreeMap::<String, Vec<semantic::Target>>::new();
        let mut proofs = BTreeMap::<String, Vec<String>>::new();
        let mut accepted = BTreeMap::<String, String>::new();
        for (raw, mut reference) in destination.reference_claims {
            let Some(prior) = captured.reference_claims.get(&raw) else {
                continue;
            };
            anyhow::ensure!(
                prior.document == reference.document && prior.range == reference.range,
                "captured reference anchor differs"
            );
            reference.roles.retain(|role| prior.roles.contains(role));
            if reference.roles.is_empty() {
                continue;
            }
            reference.resolution = prior.resolution;
            adopt_targets(
                &mut owner,
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
                &mut owner,
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
                &raw,
                semantic::EvidenceKind::SemanticReference,
            )?;
            if let Some(row) = rows.get_mut(&id) {
                let roles: BTreeSet<_> =
                    row.roles.iter().chain(&reference.roles).copied().collect();
                owner.grow((roles.len() * std::mem::size_of::<semantic::Role>()) as u64)?;
                row.roles = roles.into_iter().collect();
                if row.roles.contains(&semantic::Role::Definition) {
                    row.site = semantic::ReferenceSite::Declaration;
                }
            } else {
                entry::<String, semantic::Reference>(&mut owner, id.len() as u64)?;
                rows.insert(id, reference);
            }
        }
        // Final references in reference-ID order; the emitted row is what calls copy.
        let mut emitted = BTreeMap::<String, semantic::Reference>::new();
        for (id, mut reference) in rows {
            let mut united = targets.remove(&id).unwrap_or_default();
            canonical_targets(&self.ctx.live, &mut united)?;
            reference.declared_target = None;
            reference.candidates.clear();
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
            reference.provenance_id = self.id_for(&raw, kind);
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("reference", &mut reference)?;
            self.check_row(&view, &raw, kind, Some(&reference.document), |v, p, f| {
                v.reference(&reference, p, f)
            })?;
            let body = to_json(&reference)?;
            self.emit(
                "out_references",
                "INSERT INTO out_references VALUES(?1,?2,?3,?4)",
                &[
                    &self.next_ord(REFERENCES),
                    &reference.id,
                    &reference.provenance_id,
                    &body,
                ],
            )?;
            entry::<String, semantic::Reference>(&mut owner, id.len() as u64)?;
            emitted.insert(id, reference);
        }

        // Calls take the captured reference's targets, then their final reference's.
        let mut old_targets = BTreeMap::<&str, &semantic::Reference>::new();
        for binding in &captured.call_bindings {
            if let Some(reference) = captured.reference_claims.get(&binding.provenance_id) {
                entry::<&str, &semantic::Reference>(&mut owner, 0)?;
                old_targets.insert(binding.provenance_id.as_str(), reference);
            }
        }
        let mut calls = destination.call_bindings;
        let mut kept = 0;
        for index in 0..calls.len() {
            let Some(old) = old_targets.get(calls[index].provenance_id.as_str()) else {
                continue;
            };
            // The working copy, then the captured reference's targets it adopts.
            owner.grow(sizes::call(&calls[index])?)?;
            let mut call = calls[index].clone();
            call.resolution = old.resolution;
            adopt_targets(
                &mut owner,
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
                &mut owner,
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
                &mut owner,
                &mut call.declared_target,
                &mut call.candidates,
                reference.declared_target.as_ref(),
                &reference.candidates,
            )?;
            call.stale_target = call.declared_target.as_ref().and_then(|target| {
                matches!(target, semantic::Target::Internal { .. }).then_some(false)
            });
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("callResolution", &mut call)?;
            calls[kept] = call;
            kept += 1;
        }
        calls.truncate(kept);
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
            owner.grow(sizes::targets(union)?)?;
            call.resolution = semantic::Resolution::Ambiguous;
            call.declared_target = None;
            call.candidates = union.clone();
            call.stale_target = None;
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
            call.provenance_id = self.id_for(&raw, kind);
            #[cfg(feature = "test-support")]
            crate::semantic_batch::testing::edit_row("callBinding", &mut call)?;
            self.check_row(
                &view,
                &raw,
                kind,
                Some(&call.join.anchor.document),
                |v, p, f| v.call_binding(&call, p, f),
            )?;
            let body = to_json(&call)?;
            self.emit(
                "out_call_bindings",
                "INSERT INTO out_call_bindings VALUES(?1,?2,?3)",
                &[&self.next_ord(CALLS), &call.provenance_id, &body],
            )?;
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
                relationship.provenance_id = self.id_for(&raw, kind);
                #[cfg(feature = "test-support")]
                crate::semantic_batch::testing::edit_row("typeRelationship", &mut relationship)?;
                self.check_row(&view, &raw, kind, None, |v, p, f| {
                    v.type_relationship(&relationship, p, f)
                })?;
                let body = to_json(&relationship)?;
                self.emit(
                    "out_type_relationships",
                    "INSERT INTO out_type_relationships VALUES(?1,?2,?3)",
                    &[
                        &self.next_ord(RELATIONSHIPS),
                        &relationship.provenance_id,
                        &body,
                    ],
                )?;
            } else {
                outcome.omitted = true;
            }
        }

        self.provenance(&view, doc, &sources, destination_manifest_hash)?;
        for value in outcomes.values() {
            match value {
                1 => outcome.matched += 1,
                3 => outcome.ambiguous += 1,
                _ => {}
            }
        }
        Ok(outcome)
    }

    /// Writes this document's provenance in (raw ID, evidence kind) order.
    fn provenance(
        &self,
        view: &StagedSide<'_>,
        doc: &native::DocumentKey,
        sources: &BTreeSet<(String, semantic::EvidenceKind)>,
        destination_manifest_hash: &str,
    ) -> Result<()> {
        if sources.is_empty() {
            return Ok(());
        }
        let conn = self.conn();
        let lookup = |proof: &str, kind: &str, digest: &str| -> Result<RawClaimDocument> {
            conn.prepare_cached(
                "SELECT s.language, s.path, a.sha256 FROM claims c JOIN sources s ON s.ord=c.source_ord \
                 JOIN artifacts a ON a.ordinal=c.artifact WHERE c.proof_id=?1 AND c.record_kind=?2 AND c.fact_digest=?3",
            )?
            .query_row(params![proof, kind, digest], |row| {
                Ok(RawClaimDocument {
                    language: row.get(0)?,
                    path: row.get(1)?,
                    artifact_hash: row.get(2)?,
                })
            })
            .optional()?
            .ok_or_else(|| ScipCaptureError::MissingProof.into())
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
            self.ctx.charge(1)?;
            let artifact: i64 = conn
                .prepare_cached("SELECT artifact FROM claims WHERE proof_id=?1")?
                .query_row([raw], |row| row.get(0))?;
            let body = to_json(&provenance)?;
            self.emit(
                "out_provenance",
                "INSERT INTO out_provenance VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                &[
                    &self.next_ord(PROVENANCE),
                    &provenance.id,
                    &artifact,
                    raw,
                    &claim.record_kind,
                    &claim.fact_digest,
                    &kind_rank(*kind),
                    &body,
                ],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Room for a copy of the replaced set, not of the incoming one: denied, nothing copied.
        let budget = LiveBudget::new(incoming - 1);
        let mut owner = budget.reserve(0).unwrap();
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
        assert_eq!(budget.high_water(), 0, "nothing was admitted or copied");

        let budget = LiveBudget::new(incoming);
        let mut owner = budget.reserve(0).unwrap();
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
        assert_eq!(budget.high_water(), incoming);
    }
}
