//! #853: a Sequence view's client calls, read against the HTTP interface evidence of the
//! view's own revision. A call matched to a route of another app gets a labelled boundary hop
//! into the handler's app; a call matched to no route stays a boundary and only says so.
//! Interface evidence is structural (#788): never proof that a request is sent.
use crate::architecture_core::ArchitectureCore;
use crate::behavior::{
    InterfaceHop, InterfacesState, Participant, SequenceColumn, SequenceInterfaces, SequenceStep,
    SequenceView,
};
use crate::interfaces::{Confidence, SequenceHop};
use crate::store::EvidenceResponse;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// The interface evidence a Sequence read found for its revision.
pub(crate) enum InterfaceEvidence<'c> {
    /// The architecture core of the view's revision, built under `config`, the current rules file.
    Core {
        config: Option<&'c crate::rules::Config>,
        core: &'c ArchitectureCore,
    },
    /// No core of this revision and rules file is published.
    NotMeasured,
    /// The rules file could not be read.
    Unavailable,
}

/// What a hop's participant kind is: the server app's handler, drawn in its app's column.
pub(crate) const HANDLER_KIND: &str = "interfaceHandler";
const HANDLER_IDENTIFICATION: &str =
    "HTTP interface handler: a structural method and path match, not proof.";

/// The call IDs of every step of `steps`, nested ones and callback candidates' (#888) included.
pub(crate) fn call_ids(steps: &[SequenceStep]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    crate::behavior::any_step(steps, &mut |step| {
        if let Some(id) = &step.call_id {
            ids.insert(id.clone());
        }
        false
    });
    ids
}

fn confidence(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Exact => "exact",
        Confidence::Template => "template",
        Confidence::WrapperInferred => "wrapperInferred",
        Confidence::Unresolved => "unresolved",
    }
}

impl EvidenceResponse {
    /// Annotates `view`'s client calls with their HTTP interfaces (#853) and records how they
    /// were read in `view.interfaces`. A view with no call is left as it is. Only a matched call
    /// between two apps changes the drawing: it targets its handler's participant, and an
    /// incidental one is shown. Contention is returned; any other failure of the evidence is
    /// `unavailable`, never a failed Sequence.
    ///
    /// The view's bounds are unchanged (Prime2 review on #885): a handler that would need a
    /// column past [`crate::behavior::MAX_COLUMNS`] is not drawn and the view is truncated by
    /// `participants`; and when `visible_bound` holds (every view but the Interactions
    /// candidates, #663), revealed calls are kept within [`crate::behavior::MAX_STEPS`] shown
    /// steps by the same cap, truncated by `visibleSteps`. Counts describe the retained view.
    pub(crate) fn annotate_sequence_interfaces(
        &self,
        view: &mut SequenceView,
        evidence: InterfaceEvidence<'_>,
        visible_bound: bool,
        check: &mut (impl FnMut() -> Result<()> + Send),
    ) -> Result<()> {
        let ids = call_ids(&view.steps);
        if ids.is_empty() {
            return Ok(());
        }
        let mut record = |state| {
            view.interfaces = Some(SequenceInterfaces {
                state,
                hops: 0,
                unresolved: 0,
            });
        };
        let (config, core) = match evidence {
            InterfaceEvidence::Core { config, core } if core.pin == view.revision => (config, core),
            InterfaceEvidence::Core { .. } | InterfaceEvidence::NotMeasured => {
                record(InterfacesState::NotMeasured);
                return Ok(());
            }
            InterfaceEvidence::Unavailable => {
                record(InterfacesState::Unavailable);
                return Ok(());
            }
        };
        let Ok(candidates) = core.app_candidates() else {
            record(InterfacesState::Unavailable);
            return Ok(());
        };
        // Fewer than two apps: no interface is extracted, and there is nothing to say.
        if !crate::architecture_core::interfaces_extracted(config, candidates) {
            return Ok(());
        }
        let Ok(extraction) = core.interfaces() else {
            record(InterfacesState::Unavailable);
            return Ok(());
        };
        let wanted: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
        // Most views make no client call: they need no app assignment.
        if !extraction.calls.iter().any(|call| {
            call.call_id
                .as_deref()
                .is_some_and(|id| wanted.contains(id))
        }) {
            record(InterfacesState::Ready);
            return Ok(());
        }
        let assignment = match Self::app_assignment_at(config, core, check) {
            Ok((assignment, _)) => assignment,
            Err(error) if crate::store::cli_read_retryable_contention(&error) => return Err(error),
            Err(error) if error.to_string().starts_with("deadline_exceeded") => {
                return Err(error);
            }
            Err(_) => {
                record(InterfacesState::Unavailable);
                return Ok(());
            }
        };
        let hops = crate::interfaces::sequence_hops(
            extraction,
            |path| assignment.file(path),
            &wanted,
            check,
        )?;
        let app = |index: usize| assignment.apps().get(index).map(|app| app.name.clone());
        // One participant per handler, in its app's column.
        let mut handlers: BTreeMap<String, Option<String>> = BTreeMap::new();
        let mut column_limited = false;
        let mut found: BTreeMap<String, InterfaceHop> = BTreeMap::new();
        for (id, hop) in hops {
            check()?;
            let annotation = match hop {
                SequenceHop::Cross {
                    route,
                    confidence: level,
                    client_app,
                    server_app,
                } => {
                    let server = app(server_app);
                    let participant = match (&server, &route.handler.syntax_id) {
                        (Some(server), Some(syntax_id)) => {
                            let key = format!("interface:{server}:{syntax_id}");
                            match handlers.get(&key) {
                                Some(known) => known.clone(),
                                None => {
                                    let column = SequenceColumn {
                                        id: format!("app:{server}"),
                                        label: server.clone(),
                                    };
                                    // The app's column is admitted like any target's (#558).
                                    let fits =
                                        crate::behavior_columns::fits(&view.participants, &column);
                                    column_limited |= !fits;
                                    let symbol = if fits {
                                        self.symbol_at(syntax_id, Some(view.revision))?
                                            .map(|(_, symbol)| symbol)
                                    } else {
                                        None
                                    };
                                    let drawn = symbol.map(|symbol| {
                                        view.participants.push(Participant {
                                            id: key.clone(),
                                            label: symbol.name.clone(),
                                            kind: HANDLER_KIND.into(),
                                            identification: HANDLER_IDENTIFICATION.into(),
                                            declaration: Some(symbol),
                                            column: Some(column),
                                        });
                                        key.clone()
                                    });
                                    handlers.insert(key, drawn.clone());
                                    drawn
                                }
                            }
                        }
                        _ => None,
                    };
                    let kind = serde_json::to_value(route.handler.kind)?;
                    InterfaceHop {
                        kind: "http".into(),
                        method: Some(route.method.clone()),
                        path_template: Some(route.path_template.clone()),
                        confidence: confidence(level).into(),
                        client_app: app(client_app),
                        server_app: server,
                        handler_kind: kind.as_str().map(str::to_owned),
                        participant,
                    }
                }
                SequenceHop::Unresolved { call, client_app } => InterfaceHop {
                    kind: "http".into(),
                    method: call.method.clone(),
                    path_template: call.path_template.clone(),
                    confidence: confidence(Confidence::Unresolved).into(),
                    client_app: client_app.and_then(app),
                    server_app: None,
                    handler_kind: None,
                    participant: None,
                },
            };
            found.insert(id, annotation);
        }
        // An incidental call (`fetch`, an external client) is hidden by default; its hop is the
        // interaction, so it is shown, with every hidden step around it (a call-chain group
        // whose calls were all hidden, Prime2 second review on #885): renderers never descend
        // into a hidden step. Before the cap below, so revealed steps count toward it.
        fn reveal(steps: &mut [SequenceStep], found: &BTreeMap<String, InterfaceHop>) -> bool {
            let mut revealed = false;
            for step in steps {
                let mut inner = reveal(&mut step.children, found);
                inner |= reveal(&mut step.alternate, found);
                if let Some(hop) = step.call_id.as_deref().and_then(|id| found.get(id)) {
                    if hop.participant.is_some() && step.hidden && step.incidental.is_some() {
                        step.incidental = None;
                        step.hidden = false;
                        revealed = true;
                    }
                    step.interface = Some(hop.clone());
                }
                if inner && step.hidden {
                    step.hidden = false;
                    step.incidental = None;
                }
                revealed |= inner;
            }
            revealed
        }
        // #888: a callback whose body holds a drawn hop is shown under the call it is passed
        // to, within the view's bounds; every other candidate stays the boundary it was.
        let drawn = |step: &SequenceStep| {
            step.call_id
                .as_deref()
                .and_then(|id| found.get(id))
                .is_some_and(|hop| hop.participant.is_some())
        };
        let spliced = crate::behavior::splice_callbacks(view, &drawn, visible_bound);
        let revealed = reveal(&mut view.steps, &found) || spliced;
        if column_limited {
            view.limit(crate::behavior::SequenceLimit::Participants);
            const WARNING: &str =
                "Sequence participant limit reached; additional targets are not shown.";
            if !view.warnings.iter().any(|warning| warning == WARNING) {
                view.warnings.push(WARNING.into());
            }
        }
        if revealed && visible_bound {
            // The same typed cap as the read's own; it recounts hidden and incidental steps.
            crate::behavior::cap_visible(view, crate::behavior::MAX_STEPS);
        } else {
            view.hidden_steps = crate::behavior::hidden_count(&view.steps);
            view.incidental_steps = crate::behavior::incidental_count(&view.steps);
        }
        // Counts and handler participants describe the steps the view kept.
        let mut summary = SequenceInterfaces {
            state: InterfacesState::Ready,
            hops: 0,
            unresolved: 0,
        };
        let mut drawn = BTreeSet::new();
        let mut pending: Vec<&SequenceStep> = view.steps.iter().collect();
        while let Some(step) = pending.pop() {
            if let Some(hop) = &step.interface {
                if let Some(participant) = &hop.participant {
                    summary.hops += 1;
                    drawn.insert(participant.clone());
                } else if hop.server_app.is_none() {
                    summary.unresolved += 1;
                }
            }
            pending.extend(step.children.iter().chain(&step.alternate));
        }
        view.participants.retain(|participant| {
            participant.kind != HANDLER_KIND || drawn.contains(&participant.id)
        });
        view.interfaces = Some(summary);
        Ok(())
    }
}

#[cfg(test)]
#[path = "sequence_callback_hops_tests.rs"]
mod sequence_callback_hops_tests;
#[cfg(test)]
#[path = "sequence_hops_tests.rs"]
mod sequence_hops_tests;
