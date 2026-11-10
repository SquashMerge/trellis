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
            &mut || Ok(()),
        )
        .unwrap();
    let found = hops(&view);
    let summary: Vec<(usize, &str, Option<&str>, Option<&str>, Option<&str>)> = found
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
        .annotate_sequence_interfaces(&mut view, InterfaceEvidence::NotMeasured, &mut || Ok(()))
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
            &mut || Ok(()),
        )
        .unwrap();
    assert!(hops(&view).is_empty());
    assert_eq!(
        view.interfaces.unwrap().state,
        InterfacesState::NotExtracted
    );
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
