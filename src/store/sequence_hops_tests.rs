//! #853: a Sequence's client calls read against the HTTP interfaces of a published two-app
//! checkout: a Rust axum daemon (`alpha`) and a JavaScript web app (`web-app`).
use super::*;
use crate::behavior::{InterfaceHop, InterfacesState, SequenceStep, SequenceView};
use crate::indexer::{IndexOptions, index_workspace_bundle};
use std::{fs, sync::atomic::AtomicBool};

fn write(root: &std::path::Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

const SERVER: &str = "async fn sequence() {}\nasync fn item() {}\nasync fn head() {}\n\
fn router() -> Router {\n    Router::new()\n        .route(\"/api/sequence\", post(sequence))\n        \
.route(\"/api/items/{id}\", get(item))\n        .route(\"/api/checkouts/{root}/status/head\", get(head))\n}\n\
fn main() {}\n";

/// One call per confidence, plus two that stay boundaries: an unreadable path and a path no
/// route answers.
const CLIENT: &str = "function api(path, method) { return fetch(path, {method}); }\n\
async function load(id, dynamic) {\n  await api(\"/api/sequence\", \"POST\");\n  \
await api(`/api/items/${id}`, \"GET\");\n  await api(\"/api/status/head\", \"GET\");\n  \
await api(dynamic, \"GET\");\n  await api(\"/api/nowhere\", \"GET\");\n}\n\
function local() { return load(1, \"x\"); }\n";

fn publish(state: &tempfile::TempDir, dir: &std::path::Path) -> Store {
    let store = crate::testing::retry_admission(|| Store::open_for_tests(state.path(), dir));
    let cancel = Arc::new(AtomicBool::new(false));
    let (graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(dir.into()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &store.writer().unwrap(),
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    store
}

fn two_apps() -> (tempfile::TempDir, tempfile::TempDir, Store) {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    write(dir, "Cargo.toml", "[package]\nname = \"alpha\"\n");
    write(dir, "src/main.rs", SERVER);
    write(dir, "web/package.json", "{\"name\":\"web-app\"}\n");
    write(dir, "web/app.js", CLIENT);
    let store = publish(&state, dir);
    (state, root, store)
}

fn core(response: &EvidenceResponse) -> ArchitectureCore {
    let pin = response.selected_pin(None).unwrap();
    response
        .architecture_core_at(
            pin,
            None,
            false,
            &mut || Ok(()),
            &|_: ArchitectureProgress| {},
        )
        .unwrap()
}

fn sequence(response: &EvidenceResponse, name: &str) -> SequenceView {
    let pin = response.selected_pin(None).unwrap();
    let (_, symbols) = response.symbols_at(name, 20).unwrap();
    let seed = symbols
        .iter()
        .find(|symbol| symbol.name == name)
        .unwrap_or_else(|| panic!("{name} in {symbols:?}"));
    response.sequence_at(&seed.id, pin, false).unwrap().unwrap()
}

/// Each annotated step, by the line its call is on.
fn hops(view: &SequenceView) -> BTreeMap<usize, InterfaceHop> {
    let mut found = BTreeMap::new();
    let mut pending: Vec<&SequenceStep> = view.steps.iter().collect();
    while let Some(step) = pending.pop() {
        if let Some(hop) = &step.interface {
            found.insert(step.range.start_line, hop.clone());
        }
        pending.extend(step.children.iter().chain(&step.alternate));
    }
    found
}

#[test]
fn client_calls_cross_to_the_daemon_with_their_confidence_or_stay_boundaries() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    let before = view.participants.len();
    response
        .annotate_sequence_interfaces(
            &mut view,
            InterfaceEvidence::Core {
                config: None,
                core: &core,
            },
            true,
            &mut || Ok(()),
        )
        .unwrap();
    let found = hops(&view);
    let summary: Vec<_> = found
        .iter()
        .map(|(line, hop)| {
            (
                *line,
                hop.confidence.as_str(),
                hop.method.as_deref(),
                hop.path_template.as_deref(),
                hop.server_app.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                3,
                "exact",
                Some("POST"),
                Some("/api/sequence"),
                Some("alpha")
            ),
            (
                4,
                "template",
                Some("GET"),
                Some("/api/items/{id}"),
                Some("alpha")
            ),
            (
                5,
                "wrapperInferred",
                Some("GET"),
                Some("/api/checkouts/{root}/status/head"),
                Some("alpha")
            ),
            (6, "unresolved", Some("GET"), None, None),
            (7, "unresolved", Some("GET"), Some("/api/nowhere"), None),
        ]
    );
    for hop in found.values() {
        assert_eq!(hop.kind, "http");
        assert_eq!(hop.client_app.as_deref(), Some("web-app"));
    }
    // The three hops reach three handlers, each a participant in the daemon's column.
    let handlers: Vec<(&str, &str, &str)> = view.participants[before..]
        .iter()
        .map(|participant| {
            let column = participant.column.as_ref().unwrap();
            assert_eq!(participant.kind, crate::store::sequence_hops::HANDLER_KIND);
            assert_eq!(
                Some(participant.id.as_str()),
                found
                    .values()
                    .find(|hop| hop.participant.as_deref() == Some(participant.id.as_str()))
                    .and_then(|hop| hop.participant.as_deref())
            );
            let declaration = participant.declaration.as_ref().unwrap();
            assert_eq!(declaration.path, "src/main.rs");
            (
                participant.label.as_str(),
                column.id.as_str(),
                column.label.as_str(),
            )
        })
        .collect();
    let mut names: Vec<&str> = handlers.iter().map(|(name, ..)| *name).collect();
    names.sort();
    assert_eq!(names, ["head", "item", "sequence"]);
    assert!(
        handlers
            .iter()
            .all(|(_, column, label)| *column == "app:alpha" && *label == "alpha")
    );
    // Unresolved calls name no handler and add no participant.
    assert!(
        found
            .values()
            .filter(|hop| hop.confidence == "unresolved")
            .all(|hop| hop.participant.is_none() && hop.handler_kind.is_none())
    );
    let read = view.interfaces.as_ref().unwrap();
    assert_eq!(
        (read.state, read.hops, read.unresolved),
        (InterfacesState::Ready, 3, 2)
    );
}

#[test]
fn a_view_with_no_client_call_is_unchanged_but_says_it_was_read() {
    // Negative control: the daemon's own router makes calls, none of them a client call.
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "router");
    let original = view.clone();
    response
        .annotate_sequence_interfaces(
            &mut view,
            InterfaceEvidence::Core {
                config: None,
                core: &core,
            },
            true,
            &mut || Ok(()),
        )
        .unwrap();
    assert!(hops(&view).is_empty());
    assert_eq!(view.participants, original.participants);
    assert_eq!(view.steps, original.steps);
    let read = view.interfaces.unwrap();
    assert_eq!(
        (read.state, read.hops, read.unresolved),
        (InterfacesState::Ready, 0, 0)
    );
}

#[test]
fn without_a_core_of_the_revision_the_calls_are_not_measured_not_unmatched() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let mut view = sequence(&response, "load");
    let original = view.clone();
    response
        .annotate_sequence_interfaces(&mut view, InterfaceEvidence::NotMeasured, true, &mut || {
            Ok(())
        })
        .unwrap();
    assert_eq!(view.steps, original.steps);
    assert_eq!(view.interfaces.unwrap().state, InterfacesState::NotMeasured);
    // A core of another revision answers nothing about this one.
    let core = core(&response);
    let mut view = original.clone();
    view.revision.index_revision += 1;
    response
        .annotate_sequence_interfaces(
            &mut view,
            InterfaceEvidence::Core {
                config: None,
                core: &core,
            },
            true,
            &mut || Ok(()),
        )
        .unwrap();
    assert!(hops(&view).is_empty());
    assert_eq!(view.interfaces.unwrap().state, InterfacesState::NotMeasured);
}

#[test]
fn one_app_extracts_no_interface() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "web/package.json", "{\"name\":\"web-app\"}\n");
    write(root.path(), "web/app.js", CLIENT);
    write(root.path(), "src/lib.rs", SERVER);
    let store = publish(&state, root.path());
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    response
        .annotate_sequence_interfaces(
            &mut view,
            InterfaceEvidence::Core {
                config: None,
                core: &core,
            },
            true,
            &mut || Ok(()),
        )
        .unwrap();
    assert!(hops(&view).is_empty());
    // Nothing to say: the one-app wire is unchanged.
    assert_eq!(view.interfaces, None);
}

#[test]
fn a_hidden_incidental_client_call_with_a_hop_is_shown() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    // As an external-bound client call (`fetch`, reqwest) arrives: hidden and incidental.
    let step = view
        .steps
        .iter_mut()
        .find(|step| step.range.start_line == 3 && step.call_id.is_some())
        .expect("the exact call's step");
    step.hidden = true;
    step.incidental = Some(crate::behavior::IncidentalReason::External);
    view.hidden_steps += 1;
    view.incidental_steps += 1;
    let (hidden, incidental) = (view.hidden_steps, view.incidental_steps);
    response
        .annotate_sequence_interfaces(
            &mut view,
            InterfaceEvidence::Core {
                config: None,
                core: &core,
            },
            true,
            &mut || Ok(()),
        )
        .unwrap();
    let step = view
        .steps
        .iter()
        .find(|step| step.range.start_line == 3 && step.call_id.is_some())
        .unwrap();
    assert!(!step.hidden && step.incidental.is_none());
    assert_eq!(
        (view.hidden_steps, view.incidental_steps),
        (hidden - 1, incidental - 1)
    );
}

#[test]
fn the_hop_fields_serialize_as_the_browser_reads_them() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    response
        .annotate_sequence_interfaces(
            &mut view,
            InterfaceEvidence::Core {
                config: None,
                core: &core,
            },
            true,
            &mut || Ok(()),
        )
        .unwrap();
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(
        json["interfaces"],
        serde_json::json!({"state": "ready", "hops": 3, "unresolved": 2})
    );
    let step = json["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["interface"]["confidence"] == "exact")
        .unwrap();
    let hop = &step["interface"];
    assert_eq!(
        (
            &hop["kind"],
            &hop["method"],
            &hop["pathTemplate"],
            &hop["clientApp"],
            &hop["serverApp"],
            &hop["handlerKind"]
        ),
        (
            &serde_json::json!("http"),
            &serde_json::json!("POST"),
            &serde_json::json!("/api/sequence"),
            &serde_json::json!("web-app"),
            &serde_json::json!("alpha"),
            &serde_json::json!("declaration")
        )
    );
    assert!(
        hop["participant"]
            .as_str()
            .unwrap()
            .starts_with("interface:alpha:")
    );
}

fn annotate(response: &EvidenceResponse, core: &ArchitectureCore, view: &mut SequenceView) {
    response
        .annotate_sequence_interfaces(
            view,
            InterfaceEvidence::Core { config: None, core },
            true,
            &mut || Ok(()),
        )
        .unwrap();
}

fn shown(steps: &[SequenceStep]) -> usize {
    steps
        .iter()
        .map(|step| usize::from(!step.hidden) + shown(&step.children) + shown(&step.alternate))
        .sum()
}

#[test]
fn a_hop_never_adds_a_twenty_first_column() {
    // Prime2 review on #885: twenty drawn columns leave no room for the server app's column.
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    let filler = view.participants[0].clone();
    for at in 0..crate::behavior::MAX_COLUMNS {
        let mut participant = filler.clone();
        participant.id = format!("filler:{at}");
        participant.column = Some(crate::behavior::SequenceColumn {
            id: format!("module:filler{at}"),
            label: format!("filler{at}"),
        });
        view.participants.push(participant);
    }
    let before = view.participants.clone();
    annotate(&response, &core, &mut view);
    assert_eq!(
        view.participants, before,
        "no handler participant, no new column"
    );
    assert!(
        view.truncated
            && view
                .truncated_by
                .contains(&crate::behavior::SequenceLimit::Participants)
    );
    assert!(
        view.warnings
            .iter()
            .any(|w| w.contains("participant limit"))
    );
    let found = hops(&view);
    // The matches are still told, but none is drawn: no participant.
    assert!(found.values().all(|hop| hop.participant.is_none()));
    assert_eq!(
        found
            .values()
            .filter(|hop| hop.server_app.is_some())
            .count(),
        3
    );
    let read = view.interfaces.unwrap();
    assert_eq!((read.hops, read.unresolved), (0, 2));
}

#[test]
fn a_revealed_hop_stays_within_the_shown_step_bound() {
    // Prime2 review on #885: a hidden client call at shown step 201 is not revealed past the cap.
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    let template = view
        .steps
        .iter()
        .find(|step| step.call_id.is_none())
        .cloned()
        .unwrap_or_else(|| view.steps[0].clone());
    for step in &mut view.steps {
        if step.call_id.is_some() {
            step.hidden = true;
            step.incidental = Some(crate::behavior::IncidentalReason::External);
        }
    }
    let mut filler = Vec::new();
    for at in 0..crate::behavior::MAX_STEPS - shown(&view.steps) {
        let mut step = template.clone();
        step.id = format!("filler:{at}");
        step.kind = "effect".into();
        step.call_id = None;
        step.hidden = false;
        step.incidental = None;
        step.children.clear();
        step.alternate.clear();
        filler.push(step);
    }
    filler.append(&mut view.steps);
    view.steps = filler;
    view.hidden_steps = crate::behavior::hidden_count(&view.steps);
    view.incidental_steps = crate::behavior::incidental_count(&view.steps);
    assert_eq!(shown(&view.steps), crate::behavior::MAX_STEPS);
    annotate(&response, &core, &mut view);
    assert!(shown(&view.steps) <= crate::behavior::MAX_STEPS);
    assert!(
        view.truncated_by
            .contains(&crate::behavior::SequenceLimit::VisibleSteps)
    );
    assert_eq!(
        view.hidden_steps,
        crate::behavior::hidden_count(&view.steps)
    );
    assert_eq!(
        view.incidental_steps,
        crate::behavior::incidental_count(&view.steps)
    );
    // Counts and handler participants describe the retained view only.
    let found = hops(&view);
    let read = view.interfaces.as_ref().unwrap();
    assert_eq!(
        read.hops,
        found
            .values()
            .filter(|hop| hop.participant.is_some())
            .count()
    );
    let drawn: Vec<&str> = found
        .values()
        .filter_map(|hop| hop.participant.as_deref())
        .collect();
    assert!(
        view.participants
            .iter()
            .filter(|participant| participant.kind == crate::store::sequence_hops::HANDLER_KIND)
            .all(|participant| drawn.contains(&participant.id.as_str()))
    );
}

#[test]
fn the_interactions_candidates_keep_their_unbounded_shown_steps() {
    // Interactions bound arrows in the browser (#663): without the visible bound, a hop is
    // revealed wherever it is.
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    for step in &mut view.steps {
        if step.call_id.is_some() {
            step.hidden = true;
            step.incidental = Some(crate::behavior::IncidentalReason::External);
        }
    }
    response
        .annotate_sequence_interfaces(
            &mut view,
            InterfaceEvidence::Core {
                config: None,
                core: &core,
            },
            false,
            &mut || Ok(()),
        )
        .unwrap();
    assert!(
        !view
            .truncated_by
            .contains(&crate::behavior::SequenceLimit::VisibleSteps)
    );
    assert_eq!(view.interfaces.unwrap().hops, 3);
}

/// Calls reachable in a drawing: renderers never descend into a hidden step.
fn drawn_hops(steps: &[SequenceStep]) -> usize {
    steps
        .iter()
        .filter(|step| !step.hidden)
        .map(|step| {
            usize::from(
                step.interface
                    .as_ref()
                    .is_some_and(|hop| hop.participant.is_some()),
            ) + drawn_hops(&step.children)
                + drawn_hops(&step.alternate)
        })
        .sum()
}

#[test]
fn a_hop_inside_a_hidden_chain_group_reveals_the_group() {
    // Prime2 second review on #885: `client.get(..).header(..).send()` is one call-chain group;
    // semantic enrichment hides a group whose external calls are all hidden. Revealing the
    // matched `get` must reveal the group around it, or neither renderer reaches the hop.
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    write(
        dir,
        "Cargo.toml",
        "[workspace]\nmembers = [\"server\", \"client\"]\n",
    );
    write(dir, "server/Cargo.toml", "[package]\nname = \"server\"\n");
    write(
        dir,
        "server/src/main.rs",
        "async fn health() {}\nfn router() -> Router {\n    Router::new().route(\"/health\", get(health))\n}\nfn main() {}\n",
    );
    write(dir, "client/Cargo.toml", "[package]\nname = \"client\"\n");
    write(
        dir,
        "client/src/main.rs",
        "use reqwest::Client;\nasync fn run(client: Client) {\n    client.get(\"/health\").header(\"x\", \"v\").send().await;\n}\nfn main() {}\n",
    );
    let store = publish(&state, dir);
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "run");
    // As semantic enrichment leaves it: the chain's external calls hidden, so the group too.
    let kinds = view_kinds(&view);
    let group = view
        .steps
        .iter_mut()
        .find(|step| step.kind == "group")
        .unwrap_or_else(|| panic!("a chain group: {kinds:?}"));
    group.hidden = true;
    for child in &mut group.children {
        child.hidden = true;
        child.incidental = Some(crate::behavior::IncidentalReason::External);
    }
    view.hidden_steps = crate::behavior::hidden_count(&view.steps);
    view.incidental_steps = crate::behavior::incidental_count(&view.steps);
    annotate(&response, &core, &mut view);
    let read = view.interfaces.clone().unwrap();
    assert_eq!(read.hops, 1, "{:?}", hops(&view));
    let group = view.steps.iter().find(|step| step.kind == "group").unwrap();
    assert!(!group.hidden, "the group around the revealed call is shown");
    let get = group
        .children
        .iter()
        .find(|child| child.interface.is_some())
        .unwrap();
    assert!(!get.hidden);
    assert_eq!(
        get.interface.as_ref().unwrap().server_app.as_deref(),
        Some("server")
    );
    // The other chain calls stay hidden incidental calls.
    assert!(
        group
            .children
            .iter()
            .filter(|child| child.interface.is_none())
            .all(|child| child.hidden && child.incidental.is_some())
    );
    // What the renderers can reach is what the status line says.
    assert_eq!(drawn_hops(&view.steps), read.hops);
    assert_eq!(
        view.hidden_steps,
        crate::behavior::hidden_count(&view.steps)
    );
    assert_eq!(
        view.incidental_steps,
        crate::behavior::incidental_count(&view.steps)
    );
}

fn view_kinds(view: &SequenceView) -> Vec<(String, String)> {
    view.steps
        .iter()
        .map(|step| (step.kind.clone(), step.label.clone()))
        .collect()
}
