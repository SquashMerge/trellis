//! #888 (option C): an HTTP interface hop (#853) inside a callback. The callback's body is
//! shown, under the call it is passed to, only when it holds a drawn hop (or an interaction);
//! any other callback stays the boundary it always was. Syntax-only views, real extraction.
use super::sequence_hops_tests::{SERVER, annotate, core, hops, publish, sequence, shown, write};
use super::*;
use crate::behavior::{CALLBACK_KIND, SequenceLimit, SequenceStep, SequenceView};

/// `load` calls `api` inside the callback it passes to `browseRequest`, as `loadSequence` in
/// Trellis's own web app does; `quiet` passes a callback that makes no client call.
const CLIENT: &str = "function api(path, method) { return fetch(path, {method}); }\n\
async function browseRequest(action) { await action(); }\n\
async function load(id) {\n  await browseRequest(async () => {\n    await api(\"/api/sequence\", \"POST\");\n  });\n}\n\
async function quiet() {\n  await browseRequest(async () => {\n    local();\n  });\n}\n\
async function nested() {\n  await browseRequest(async () => {\n    await browseRequest(async () => {\n      await api(`/api/items/${7}`, \"GET\");\n    });\n  });\n}\n\
function local() {}\n";

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

fn all(steps: &[SequenceStep]) -> Vec<&SequenceStep> {
    steps
        .iter()
        .flat_map(|step| {
            std::iter::once(step)
                .chain(all(&step.children))
                .chain(all(&step.alternate))
        })
        .collect()
}

fn blocks(view: &SequenceView) -> Vec<&SequenceStep> {
    all(&view.steps)
        .into_iter()
        .filter(|step| step.kind == CALLBACK_KIND)
        .collect()
}

/// Every candidate dropped, as the wire carries the view.
fn without_candidates(steps: &mut [SequenceStep]) {
    for step in steps {
        step.callback = None;
        without_candidates(&mut step.children);
        without_candidates(&mut step.alternate);
    }
}

const BOUNDARY: &str = "Nested function/callback/class boundary";

#[test]
fn a_hop_inside_a_callback_is_shown_under_the_call_it_is_passed_to() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    // Before the read: the syntax view keeps its boundary, holding the built body.
    let boundary = view
        .steps
        .iter()
        .find(|step| step.label.starts_with(BOUNDARY))
        .unwrap();
    assert!(boundary.callback.is_some());
    assert!(blocks(&view).is_empty());
    annotate(&response, &core, &mut view);
    let read = view.interfaces.clone().unwrap();
    assert_eq!((read.hops, read.unresolved), (1, 0));
    // The block sits under `browseRequest`, in the seed's own lane.
    let call = view
        .steps
        .iter()
        .find(|step| step.kind == "call" && step.label == "browseRequest")
        .unwrap();
    let block = call.children.last().unwrap();
    assert_eq!(block.kind, CALLBACK_KIND);
    assert_eq!(
        block.label,
        "callback passed to browseRequest · when and how often it runs is unknown"
    );
    assert_eq!(block.lane, None);
    assert_eq!((block.range.start_line, block.range.end_line), (4, 6));
    let hop = all(&block.children)
        .into_iter()
        .find_map(|step| step.interface.clone())
        .unwrap();
    assert_eq!(
        (hop.method.as_deref(), hop.path_template.as_deref()),
        (Some("POST"), Some("/api/sequence"))
    );
    assert_eq!(hop.server_app.as_deref(), Some("alpha"));
    assert!(
        view.participants
            .iter()
            .any(|p| Some(&p.id) == hop.participant.as_ref())
    );
    assert!(
        !view
            .steps
            .iter()
            .any(|step| step.label.starts_with(BOUNDARY))
    );
    assert_eq!(hops(&view).len(), 1);
    assert!(!view.truncated, "{:?}", view.warnings);
    assert_eq!(
        view.hidden_steps,
        crate::behavior::hidden_count(&view.steps)
    );
}

/// Negative control: a callback with no hop is not expanded, and the read leaves the view
/// exactly as it was.
#[test]
fn a_callback_without_a_hop_stays_a_boundary() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "quiet");
    let mut before = view.clone();
    without_candidates(&mut before.steps);
    annotate(&response, &core, &mut view);
    assert!(blocks(&view).is_empty());
    let read = view.interfaces.take().unwrap();
    assert_eq!((read.hops, read.unresolved), (0, 0));
    assert_eq!(
        serde_json::to_value(&view).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    // A candidate the read did not show is never on the wire.
    without_candidates(&mut view.steps);
    assert_eq!(view.steps, before.steps);
}

#[test]
fn a_hop_inside_a_nested_callback_shows_both_blocks() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "nested");
    annotate(&response, &core, &mut view);
    let found = blocks(&view);
    assert_eq!(found.len(), 2, "{:#?}", view.steps);
    assert_eq!(view.interfaces.as_ref().unwrap().hops, 1);
    let hop = hops(&view).into_values().next().unwrap();
    assert_eq!(hop.path_template.as_deref(), Some("/api/items/{id}"));
}

/// Fills `view` with shown steps up to the visible bound, ahead of its own.
fn fill(view: &mut SequenceView) {
    let template = view.steps[0].clone();
    let mut filler: Vec<SequenceStep> = (0..crate::behavior::MAX_STEPS - shown(&view.steps))
        .map(|at| SequenceStep {
            id: format!("filler:{at}"),
            kind: "effect".into(),
            call_id: None,
            children: vec![],
            alternate: vec![],
            callback: None,
            ..template.clone()
        })
        .collect();
    filler.append(&mut view.steps);
    view.steps = filler;
    assert_eq!(shown(&view.steps), crate::behavior::MAX_STEPS);
}

#[test]
fn a_callback_hop_never_takes_a_view_past_its_shown_step_bound() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    fill(&mut view);
    annotate(&response, &core, &mut view);
    assert!(blocks(&view).is_empty());
    assert_eq!(shown(&view.steps), crate::behavior::MAX_STEPS);
    assert!(view.truncated_by.contains(&SequenceLimit::VisibleSteps));
    // Nothing drawn is counted: the hop stayed in the boundary it was not shown from.
    let read = view.interfaces.unwrap();
    assert_eq!(read.hops, 0);
    assert!(
        view.participants
            .iter()
            .all(|p| p.kind != crate::store::sequence_hops::HANDLER_KIND)
    );

    // The Interactions candidates (#663) are not bound by shown steps.
    let mut view = sequence(&response, "load");
    fill(&mut view);
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
    assert_eq!(blocks(&view).len(), 1);
    assert_eq!(view.interfaces.unwrap().hops, 1);
}

#[test]
fn a_callback_hop_never_adds_a_twenty_first_column() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    let filler = view.participants[0].clone();
    for at in 0..crate::behavior::MAX_COLUMNS {
        view.participants.push(crate::behavior::Participant {
            id: format!("filler:{at}"),
            column: Some(crate::behavior::SequenceColumn {
                id: format!("module:filler{at}"),
                label: format!("filler{at}"),
            }),
            ..filler.clone()
        });
    }
    annotate(&response, &core, &mut view);
    // The match is not drawn, so the callback holds no drawn hop: it stays a boundary.
    assert!(blocks(&view).is_empty());
    assert!(view.truncated_by.contains(&SequenceLimit::Participants));
    assert_eq!(view.interfaces.unwrap().hops, 0);
}

/// A Rust closure passed to a call, holding a reqwest client call that hops to the server.
#[test]
fn a_hop_inside_a_rust_closure_is_shown() {
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
        "use reqwest::Client;\nfn run(client: Client) {\n    retry(|| client.get(\"/health\").send());\n}\nfn quiet() {\n    retry(|| local());\n}\nfn local() {}\nfn main() {}\n",
    );
    let store = publish(&state, dir);
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "run");
    annotate(&response, &core, &mut view);
    assert_eq!(blocks(&view).len(), 1, "{:#?}", view.steps);
    assert_eq!(view.interfaces.as_ref().unwrap().hops, 1);
    let hop = hops(&view).into_values().next().unwrap();
    assert_eq!(hop.server_app.as_deref(), Some("server"));
    // Negative control.
    let mut quiet = sequence(&response, "quiet");
    annotate(&response, &core, &mut quiet);
    assert!(blocks(&quiet).is_empty());
    assert!(
        quiet
            .steps
            .iter()
            .any(|step| step.label.starts_with("Nested callable boundary"))
    );
}

#[test]
fn review_891_callback_exactly_fills_visible_bound() {
    let (_state, _root, store) = two_apps();
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "load");
    let before = shown(&view.steps);
    let mut positive = view.clone();
    annotate(&response, &core, &mut positive);
    assert_eq!(blocks(&positive).len(), 1);
    let delta = shown(&positive.steps) - before;
    assert!(delta > 0);
    // The current positive result tells us the true replacement cost, including await.
    fill(&mut view);
    view.steps.drain(0..delta);
    assert_eq!(shown(&view.steps), crate::behavior::MAX_STEPS - delta);
    annotate(&response, &core, &mut view);
    assert_eq!(
        blocks(&view).len(),
        1,
        "callback replacement exactly fits 200; true delta={delta}; warnings={:?}",
        view.warnings
    );
    assert_eq!(shown(&view.steps), crate::behavior::MAX_STEPS);
    assert_eq!(view.interfaces.as_ref().unwrap().hops, 1);
    assert!(!view.truncated_by.contains(&SequenceLimit::VisibleSteps));
}

#[test]
fn review891_nested_depth_refusal_reports_the_bound() {
    for nesting in [11, 12] {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let dir = root.path();
        write(dir, "Cargo.toml", "[package]\nname = \"alpha\"\n");
        write(dir, "src/main.rs", SERVER);
        write(dir, "web/package.json", "{\"name\":\"web-app\"}\n");
        let mut body = "api(\"/api/sequence\", \"POST\");".to_owned();
        for _ in 0..nesting {
            body = format!("wrap(() => {{ {body} }});");
        }
        let source = format!(
            "function api(path, method) {{ return fetch(path, {{method}}); }}\nfunction wrap(f) {{ f(); }}\nfunction run() {{ {body} }}\n"
        );
        write(dir, "web/app.js", &source);
        let store = publish(&state, dir);
        let response = store.evidence_response().unwrap();
        let core = core(&response);
        let mut view = sequence(&response, "run");
        annotate(&response, &core, &mut view);
        if nesting == 11 {
            assert_eq!(blocks(&view).len(), nesting, "{:?}", view.warnings);
            assert_eq!(view.interfaces.as_ref().unwrap().hops, 1);
            assert!(!view.truncated, "{:?}", view.warnings);
        } else {
            assert!(blocks(&view).is_empty());
            assert!(
                view.truncated_by.contains(&SequenceLimit::Depth),
                "{:?}",
                view.warnings
            );
            assert!(
                view.warnings.iter().any(|w| w
                    == "Sequence depth limit reached; the callback passed to wrap is not shown."),
                "{:?}",
                view.warnings
            );
        }
    }
}

#[test]
fn review891_candidate_work_refusal_reports_the_bound() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let dir = root.path();
    write(dir, "Cargo.toml", "[package]\nname = \"alpha\"\n");
    write(dir, "src/main.rs", SERVER);
    write(dir, "web/package.json", "{\"name\":\"web-app\"}\n");
    let body = format!(
        "{} api(\"/api/sequence\", \"POST\");",
        "console.log();\n".repeat(1000)
    );
    let source = format!(
        "function api(path, method) {{ return fetch(path, {{method}}); }}\nfunction wrap(f) {{ f(); }}\nfunction run() {{ wrap(() => {{ {body} }}); wrap(() => {{ {body} }}); }}\n"
    );
    write(dir, "web/app.js", &source);
    let store = publish(&state, dir);
    let response = store.evidence_response().unwrap();
    let core = core(&response);
    let mut view = sequence(&response, "run");
    annotate(&response, &core, &mut view);
    assert!(
        view.truncated_by.contains(&SequenceLimit::WorkSteps),
        "{:?}",
        view.warnings
    );
    assert!(
        view.warnings
            .iter()
            .any(|w| w.contains("work limit") && w.contains("callback passed to wrap")),
        "{:?}",
        view.warnings
    );
}
