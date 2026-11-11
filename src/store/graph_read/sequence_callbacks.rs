//! #888 (owner ruling 2026-11-10, option C): the body of a callable passed to a measured call
//! (`browseRequest(async () => api(...))`, `items.map(|x| client.get(x))`) is shown, as a
//! callback block under that call, only when it holds something the view would draw: an
//! interaction (a call whose exact target is in another column) or an HTTP interface hop
//! (#853). Every other callback stays the boundary it always was, so those diagrams are
//! unchanged.
//!
//! An interaction is decided here, after the view's own enrichment, so the view's own steps
//! and bounds are untouched. A hop is decided by the read that has interface evidence: a
//! callback with calls but no interaction keeps its built block on the boundary as a
//! candidate ([`crate::behavior::CallbackCandidate`]) for [`crate::behavior::splice_callbacks`].
//! The language-neutral rule needs an indexed callable at the boundary's exact range, which
//! JavaScript, TypeScript and Rust (closures), Python (lambdas) and Java (lambdas) index.
use super::*;
use crate::behavior::{
    CallbackCandidate, MAX_STEP_DEPTH, MAX_STEPS, MAX_WORK_STEPS, SequenceLimit, SequenceStep,
    callback_block, interacts, place_callback, step_count, tree_depth, visible_count,
};

/// The callback bodies one view builds at most: each is a parse of its file. Hostile input
/// with many callbacks stays bounded; later callbacks stay boundaries.
const MAX_CALLBACKS: usize = 64;

/// What a view's callbacks are read from: each owner document once.
#[derive(Default)]
pub(super) struct CallbackReader {
    documents: BTreeMap<String, SelectedDocument>,
    /// Each owner document's syntax, parsed once, for [`crate::behavior::passed_callable`].
    trees: BTreeMap<String, Option<tree_sitter::Tree>>,
    built: usize,
    /// Steps built for candidates the view does not show: they share the work bound.
    work: usize,
}

/// A callable passed to a measured call, and its body as built.
struct Built {
    symbol: Symbol,
    outer: String,
    callee: String,
    body: crate::behavior::SequenceView,
}

impl CallbackReader {
    /// The callback at `step`, a boundary of `owner`'s body, with its body built: a function
    /// indexed at exactly the boundary's range, inside one of `owner`'s measured calls.
    fn build(
        &mut self,
        (store, db, selected): (&Store, &Connection, &ReadRevision),
        revision: IndexPin,
        show_all: bool,
        step: &SequenceStep,
        owner: &str,
    ) -> Result<Option<Built>> {
        if step.kind != "boundary"
            || step.call_id.is_some()
            || step.hidden
            || !step.children.is_empty()
            || !step.alternate.is_empty()
            || step.callback.is_some()
            || self.built >= MAX_CALLBACKS
        {
            return Ok(None);
        }
        if !self.documents.contains_key(&step.path) {
            let mut document = store.selected_graph_document_for(db, &step.path, selected)?;
            document
                .graph_calls
                .sort_by(|a, b| (a.range.start_byte, &a.id).cmp(&(b.range.start_byte, &b.id)));
            self.documents.insert(step.path.clone(), document);
        }
        let document = &self.documents[&step.path];
        let (start, end) = (step.range.start_byte, step.range.end_byte);
        let Some(symbol) = document.nodes.iter().find(|node| {
            node.kind == SymbolKind::Function
                && node.id != owner
                && node.path == step.path
                && node.range.start_byte == start
                && node.range.end_byte == end
        }) else {
            return Ok(None);
        };
        // Only a callable that is itself an argument is passed (Prime review on #891).
        let tree = self
            .trees
            .entry(step.path.clone())
            .or_insert_with(|| crate::behavior::callback_tree(&document.file));
        if !tree
            .as_ref()
            .is_some_and(|tree| crate::behavior::passed_callable(tree, start, end))
        {
            return Ok(None);
        }
        // The innermost of `owner`'s measured calls whose source holds the callable.
        let Some(outer) = document
            .graph_calls
            .iter()
            .filter(|call| {
                call.caller == owner
                    && call.path == step.path
                    && call.range.start_byte <= start
                    && end <= call.range.end_byte
                    && call.range.end_byte - call.range.start_byte > end - start
            })
            .min_by_key(|call| call.range.end_byte - call.range.start_byte)
        else {
            return Ok(None);
        };
        self.built += 1;
        let body = crate::behavior::build_sequence_work(
            revision,
            symbol,
            &document.file,
            &document.graph_calls,
            show_all,
        )?;
        Ok(Some(Built {
            symbol: symbol.clone(),
            outer: outer.id.clone(),
            callee: outer
                .callee_text
                .clone()
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| "the call".into()),
            body,
        }))
    }
}

/// Gives the steps of a callback body at `boundary` their own identities and the boundary's
/// lane: the callback runs in its definer's column.
fn relabel(steps: &mut [SequenceStep], boundary: &SequenceStep) {
    fn walk(steps: &mut [SequenceStep], prefix: &str, lane: &Option<String>) {
        for step in steps {
            step.id = format!("{prefix}>{}", step.id);
            step.lane.clone_from(lane);
            walk(&mut step.children, prefix, lane);
            walk(&mut step.alternate, prefix, lane);
            walk(&mut step.folded, prefix, lane);
        }
    }
    walk(steps, &format!("{}>callback", boundary.id), &boundary.lane);
}

/// Whether `steps` hold a measured call: only then can a read find a hop in them.
fn has_call(steps: &[SequenceStep]) -> bool {
    crate::behavior::any_step(steps, &mut |step| step.call_id.is_some())
}

/// Keeps a callback `limit` refused as a refused candidate when its body holds a measured
/// call (Prime review on #891): a read that finds a drawn hop among them reports the bound.
fn refuse(
    step: &mut SequenceStep,
    callee: &str,
    outer: String,
    body: &[SequenceStep],
    limit: SequenceLimit,
) {
    if has_call(body) {
        let candidate = CallbackCandidate::refused(step, callee, outer, body, limit);
        step.callback = Some(Box::new(candidate));
    }
}

/// Who owns `step` in a list of `owner`'s steps drawn on `lane`: `owner`, or the expanded
/// callee whose lane it has (#522).
fn owner_of<'s>(
    step: &'s SequenceStep,
    owner: &'s str,
    lane: Option<&'s str>,
) -> (&'s str, Option<&'s str>) {
    match step.lane.as_deref() {
        Some(callee) if Some(callee) != lane => (callee, Some(callee)),
        _ => (owner, lane),
    }
}

impl Enrichment<'_, '_> {
    /// Shows each callback in `steps`, measured in `owner`'s body on `lane` at tree `depth`,
    /// that holds an interaction, after the view's own enrichment (#888). Its body is enriched
    /// like an expanded callee's, in one transaction: a callback that holds no interaction
    /// leaves the view as it was, and keeps its block as a candidate when it holds a call.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn callbacks(
        &mut self,
        reader: &mut CallbackReader,
        steps: &mut Vec<SequenceStep>,
        owner: &str,
        lane: Option<&str>,
        seed: &str,
        depth: usize,
    ) -> Result<()> {
        let mut at = 0;
        while at < steps.len() {
            self.source.check_work()?;
            let step = &mut steps[at];
            if step.hidden || step.kind == crate::behavior::CALLBACK_KIND {
                at += 1;
                continue;
            }
            let (step_owner, step_lane) = owner_of(step, owner, lane);
            let (step_owner, step_lane) = (step_owner.to_owned(), step_lane.map(str::to_owned));
            self.callbacks(
                reader,
                &mut step.children,
                &step_owner,
                step_lane.as_deref(),
                seed,
                depth + 1,
            )?;
            self.callbacks(
                reader,
                &mut step.alternate,
                &step_owner,
                step_lane.as_deref(),
                seed,
                depth + 1,
            )?;
            let read = (self.store, self.db, self.selected);
            let Some(Built {
                symbol,
                outer,
                callee,
                mut body,
            }) = reader.build(read, self.revision, self.show_all, step, &step_owner)?
            else {
                at += 1;
                continue;
            };
            if body.steps.is_empty() {
                at += 1;
                continue;
            }
            if reader.work + step_count(&body.steps) > MAX_WORK_STEPS {
                // Too much to enrich: a read that finds a hop in it reports the bound.
                refuse(step, &callee, outer, &body.steps, SequenceLimit::WorkSteps);
                at += 1;
                continue;
            }
            relabel(&mut body.steps, step);
            let key = self
                .source
                .locate(&symbol.id)?
                .ok_or_else(|| SelectedIntegrity("selected sequence callback missing".into()))?;
            let facts = self.source.document(&key)?;
            let (store, db, selected) = (self.store, self.db, self.selected);
            let accessors = &mut self.accessors;
            let bodies = &mut self.bodies;
            hide_incidental(
                self.source,
                self.languages,
                self.show_all,
                &mut body.steps,
                &facts,
                &symbol.id,
                &mut BTreeSet::new(),
                &mut |id, key| expandable(store, db, selected, bodies, id, key),
                &mut |id, key| accessor(store, db, selected, accessors, id, key),
            )?;
            // One transaction: everything enriching the body adds, the view keeps only when
            // the callback is shown.
            let saved = (
                self.participants.len(),
                self.warnings.len(),
                self.nodes.len(),
                self.edges.len(),
                self.expanded.clone(),
                self.limits.clone(),
                self.truncated,
                self.used,
                self.work,
            );
            self.steps(&mut body.steps, &facts, &symbol.id, depth + 2)?;
            self.callbacks(
                reader,
                &mut body.steps,
                &symbol.id,
                step.lane.as_deref(),
                seed,
                depth + 2,
            )?;
            let step = &steps[at];
            if interacts(&body.steps, self.participants, seed) {
                // The block replaces its boundary, and the body already holds what enriching
                // it expanded: the view keeps exactly the body (Prime review on #891), counted
                // once against the counts before the transaction.
                let (used, work) = (saved.7, saved.8);
                let shown = visible_count(&body.steps);
                let built = step_count(&body.steps);
                let refused = if !self.interactions && used + shown > MAX_STEPS {
                    Some(SequenceLimit::VisibleSteps)
                } else if work + built > MAX_WORK_STEPS {
                    Some(SequenceLimit::WorkSteps)
                } else if depth + 2 + tree_depth(&body.steps) >= MAX_STEP_DEPTH {
                    Some(SequenceLimit::Depth)
                } else {
                    None
                };
                if let Some(limit) = refused {
                    self.rollback(saved);
                    self.limits.insert(limit);
                    self.warn(crate::behavior::callback_warning(limit, &callee));
                    at += 1;
                    continue;
                }
                self.used = used + shown;
                self.work = work + built;
                for warning in std::mem::take(&mut body.warnings) {
                    self.warn(warning);
                }
                self.truncated |= body.truncated;
                self.limits.extend(body.truncated_by);
                let block = callback_block(step, &callee, body.steps);
                if !place_callback(steps, at, block, &outer) {
                    at += 1;
                }
                continue;
            }
            // A target refused a column (#558) is an interaction the view cannot draw: the
            // callback stays a boundary, and the view says it was truncated by participants.
            let refused = self.limits.contains(&SequenceLimit::Participants);
            let participants = self.rollback(saved);
            if refused {
                self.limits.insert(SequenceLimit::Participants);
            }
            let step = &mut steps[at];
            if depth + 2 + tree_depth(&body.steps) >= MAX_STEP_DEPTH {
                refuse(step, &callee, outer, &body.steps, SequenceLimit::Depth);
            } else if has_call(&body.steps) {
                reader.work += step_count(&body.steps);
                let block = callback_block(step, &callee, body.steps);
                step.callback = Some(Box::new(CallbackCandidate {
                    block,
                    outer_call: outer,
                    participants,
                    callee,
                    refused: None,
                }));
            }
            at += 1;
        }
        Ok(())
    }

    /// Undoes what enriching a callback body added; returns the participants it added.
    #[allow(clippy::type_complexity)]
    fn rollback(
        &mut self,
        (participants, warnings, nodes, edges, expanded, limits, truncated, used, work): (
            usize,
            usize,
            usize,
            usize,
            BTreeSet<String>,
            BTreeSet<SequenceLimit>,
            bool,
            usize,
            usize,
        ),
    ) -> Vec<crate::behavior::Participant> {
        let added = self.participants.split_off(participants);
        self.warnings.truncate(warnings);
        self.nodes.truncate(nodes);
        self.edges.truncate(edges);
        self.expanded = expanded;
        self.limits = limits;
        self.truncated = truncated;
        self.used = used;
        self.work = work;
        added
    }
}

/// The callbacks of a syntax-only view (#888): no call has an exact target, so none holds
/// an interaction. A callback that holds a measured call keeps its built block as a
/// candidate for the read that has interface evidence; nothing else changes.
pub(in crate::store) fn syntax_callbacks(
    store: &Store,
    db: &Connection,
    selected: &ReadRevision,
    view: &mut crate::behavior::SequenceView,
    show_all: bool,
) -> Result<()> {
    fn walk(
        reader: &mut CallbackReader,
        read: (&Store, &Connection, &ReadRevision),
        revision: IndexPin,
        show_all: bool,
        steps: &mut [SequenceStep],
        owner: &str,
        depth: usize,
    ) -> Result<()> {
        for step in steps {
            if step.hidden {
                continue;
            }
            walk(
                reader,
                read,
                revision,
                show_all,
                &mut step.children,
                owner,
                depth + 1,
            )?;
            walk(
                reader,
                read,
                revision,
                show_all,
                &mut step.alternate,
                owner,
                depth + 1,
            )?;
            let Some(Built {
                symbol,
                outer,
                callee,
                mut body,
            }) = reader.build(read, revision, show_all, step, owner)?
            else {
                continue;
            };
            relabel(&mut body.steps, step);
            walk(
                reader,
                read,
                revision,
                show_all,
                &mut body.steps,
                &symbol.id,
                depth + 2,
            )?;
            let work = step_count(&body.steps);
            if !has_call(&body.steps) {
                continue;
            }
            if depth + 2 + tree_depth(&body.steps) >= MAX_STEP_DEPTH {
                refuse(step, &callee, outer, &body.steps, SequenceLimit::Depth);
            } else if reader.work + work > MAX_WORK_STEPS {
                refuse(step, &callee, outer, &body.steps, SequenceLimit::WorkSteps);
            } else {
                reader.work += work;
                let block = callback_block(step, &callee, body.steps);
                step.callback = Some(Box::new(CallbackCandidate {
                    block,
                    outer_call: outer,
                    participants: vec![],
                    callee,
                    refused: None,
                }));
            }
        }
        Ok(())
    }
    let seed = view.seed.id.clone();
    walk(
        &mut CallbackReader::default(),
        (store, db, selected),
        view.revision,
        show_all,
        &mut view.steps,
        &seed,
        0,
    )
}
