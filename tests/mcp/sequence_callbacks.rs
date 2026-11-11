#![cfg(feature = "test-support")]
//! #888 (option C): a callback's body is shown, as a block under the call it is passed to,
//! only when it holds an interaction: a call whose exact target is drawn in another column.
//! Any other callback stays the boundary it always was.
use trellis::behavior::{CALLBACK_KIND, SequenceLimit, SequenceStep, SequenceView};
use trellis::semantic_evidence as s;

use crate::graph_fixtures::{Bind, Publisher, Spec, bind, syntax_id};

fn publish_with(
    files: &[(&str, &str)],
    language: s::Language,
    bindings: Vec<Bind>,
    interactions: bool,
) -> SequenceView {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(files);
    spec.languages = vec![language];
    spec.bindings = bindings;
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    let store = &publisher.store;
    if interactions {
        store.sequence_interactions_at(&seed, pin)
    } else {
        store.sequence_at(&seed, pin, false)
    }
    .unwrap()
    .unwrap()
}

fn publish(files: &[(&str, &str)], language: s::Language, bindings: Vec<Bind>) -> SequenceView {
    publish_with(files, language, bindings, false)
}

/// Every step, nested ones included, in pre-order.
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

fn callbacks(view: &SequenceView) -> Vec<&SequenceStep> {
    all(&view.steps)
        .into_iter()
        .filter(|step| step.kind == CALLBACK_KIND)
        .collect()
}

fn target_name<'v>(view: &'v SequenceView, step: &SequenceStep) -> Option<&'v str> {
    let target = step.target.as_deref()?;
    view.participants
        .iter()
        .find(|p| p.id == target)
        .map(|p| p.label.as_str())
}

/// The callback block under the call named `outer`, and the names its calls target.
fn shown_under<'v>(view: &'v SequenceView, outer: &str) -> (&'v SequenceStep, Vec<&'v str>) {
    let call = all(&view.steps)
        .into_iter()
        .find(|step| step.kind == "call" && step.label == outer)
        .unwrap_or_else(|| panic!("no call {outer}: {:#?}", view.steps));
    let block = call
        .children
        .iter()
        .find(|step| step.kind == CALLBACK_KIND)
        .unwrap_or_else(|| panic!("no callback under {outer}: {:#?}", call.children));
    let targets = all(&block.children)
        .into_iter()
        .filter_map(|step| target_name(view, step))
        .collect();
    (block, targets)
}

const JS_WRAP: &str = "function run() {\n  wrap(() => { other.thing(); });\n}\nfunction wrap(action) { return action(); }\n";
const JS_OTHER: &str = "class Other {\n  thing() {}\n}\n";

#[test]
fn a_javascript_or_typescript_callback_with_an_interaction_is_shown_under_its_call() {
    for (language, run, other) in [
        (s::Language::Javascript, "web/run.js", "web/other.js"),
        (s::Language::Typescript, "web/run.ts", "web/other.ts"),
    ] {
        let view = publish(
            &[(run, JS_WRAP), (other, JS_OTHER)],
            language,
            vec![bind("run", 0, "wrap"), bind("anonymous#0", 0, "thing")],
        );
        let (block, targets) = shown_under(&view, "wrap");
        assert_eq!(
            block.label,
            "callback passed to wrap · when and how often it runs is unknown"
        );
        assert_eq!(targets, ["thing"], "{language:?}");
        // The callback runs in its definer's column: the seed's own steps have no lane.
        assert_eq!(block.lane, None);
        assert!(all(&block.children).iter().all(|step| step.lane.is_none()));
        assert_eq!((block.range.start_line, block.range.end_line), (2, 2));
        // The boundary it replaced is gone; nothing else is.
        assert!(
            all(&view.steps)
                .iter()
                .all(|step| !step.label.starts_with("Nested function/callback"))
        );
        assert_eq!(callbacks(&view).len(), 1);
        let thing = view
            .participants
            .iter()
            .find(|p| p.label == "thing")
            .unwrap();
        assert_eq!(thing.column.as_ref().unwrap().label, "Other");
        assert!(!view.truncated, "{:?}", view.warnings);
        // The block is a DTO step like any other.
        let json = serde_json::to_value(block).unwrap();
        assert_eq!(json["kind"], "callback");
        assert!(json.get("callback").is_none());
    }
}

/// Negative control: a callback whose calls stay in the seed's own column draws no
/// interaction, so it stays the boundary it was, and the view is the one it was before.
#[test]
fn a_callback_without_an_interaction_stays_a_boundary_and_the_view_is_unchanged() {
    let source = "function run() {\n  wrap(() => { helper(); });\n}\nfunction wrap(action) { return action(); }\nfunction helper() {}\n";
    let bound = publish(
        &[("web/run.js", source)],
        s::Language::Javascript,
        vec![bind("run", 0, "wrap"), bind("anonymous#0", 0, "helper")],
    );
    assert!(callbacks(&bound).is_empty(), "{:#?}", bound.steps);
    let boundaries: Vec<_> = all(&bound.steps)
        .into_iter()
        .filter(|step| step.label.starts_with("Nested function/callback"))
        .collect();
    assert_eq!(boundaries.len(), 1);
    assert!(boundaries[0].children.is_empty());
    let names: Vec<_> = bound
        .participants
        .iter()
        .map(|p| p.label.as_str())
        .collect();
    assert_eq!(names, ["run", "wrap"]);
    // The same view without the callback's binding: byte-identical on the wire.
    let unbound = publish(
        &[("web/run.js", source)],
        s::Language::Javascript,
        vec![bind("run", 0, "wrap")],
    );
    assert_eq!(wire(&bound), wire(&unbound));
}

/// A view's wire form without what differs between two publications of one source: its
/// revision, syntax and occurrence IDs, and semantic bases.
fn wire(view: &SequenceView) -> serde_json::Value {
    fn strip(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(map) => {
                map.remove("revision");
                map.remove("semanticBasis");
                map.values_mut().for_each(strip);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip),
            serde_json::Value::String(text)
                if text.starts_with("sid:v1:") || text.starts_with("occ:v2:") =>
            {
                *text = "id".into();
            }
            _ => {}
        }
    }
    let mut value = serde_json::to_value(view).unwrap();
    strip(&mut value);
    value
}

#[test]
fn a_rust_closure_with_an_interaction_is_shown_under_its_call() {
    let view = publish(
        &[
            (
                "src/lib.rs",
                "fn run() {\n    wrap(|| other::thing());\n}\nfn wrap(f: impl Fn()) { f() }\n",
            ),
            ("src/other.rs", "pub fn thing() {}\n"),
        ],
        s::Language::Rust,
        vec![bind("run", 0, "wrap"), bind("anonymous#0", 0, "thing")],
    );
    let (block, targets) = shown_under(&view, "wrap");
    assert_eq!(targets, ["thing"]);
    assert!(block.label.starts_with("callback passed to wrap"));
    assert_eq!(callbacks(&view).len(), 1);
}

/// A callback inside a callback: the outer is shown because the inner holds the interaction.
#[test]
fn a_nested_callback_with_an_interaction_shows_both_blocks() {
    let source = "function run() {\n  wrap(() => { wrap(() => { other.thing(); }); });\n}\nfunction wrap(action) { return action(); }\n";
    let view = publish(
        &[("web/run.js", source), ("web/other.js", JS_OTHER)],
        s::Language::Javascript,
        vec![
            bind("run", 0, "wrap"),
            bind("anonymous#0", 0, "wrap"),
            bind("anonymous#1", 0, "thing"),
        ],
    );
    let blocks = callbacks(&view);
    assert_eq!(blocks.len(), 2, "{:#?}", view.steps);
    // The inner block is under the outer block's own call to `wrap`.
    let inner: Vec<_> = all(&blocks[0].children)
        .into_iter()
        .filter(|step| step.kind == CALLBACK_KIND)
        .collect();
    assert_eq!(inner.len(), 1);
    assert_eq!(
        all(&inner[0].children)
            .into_iter()
            .filter_map(|step| target_name(&view, step))
            .collect::<Vec<_>>(),
        ["thing"]
    );
}

/// The shown-step bound (#536): a view already at 200 shown steps does not show the
/// callback, says so, and is truncated by it. The Interactions candidates (#663) are not
/// bound by shown steps, so they show it.
#[test]
fn a_callback_never_takes_a_view_past_its_shown_step_bound() {
    // `f0` is external, so hidden; the view enriches and shows 200 steps: 198 calls, the
    // callback's boundary and `wrap`.
    let filler: String = (0..199).map(|n| format!("  f{n}();\n")).collect();
    let source = format!(
        "function run() {{\n{filler}  wrap(() => {{ other.thing(); }});\n}}\nfunction wrap(action) {{ return action(); }}\n"
    );
    let files = [("web/run.js", source.as_str()), ("web/other.js", JS_OTHER)];
    let external = || Bind {
        resolution: s::Resolution::External,
        target: None,
        ..bind("run", 0, "f0")
    };
    let bindings = || vec![external(), bind("anonymous#0", 0, "thing")];
    let view = publish(&files, s::Language::Javascript, bindings());
    assert!(callbacks(&view).is_empty());
    assert!(view.truncated_by.contains(&SequenceLimit::VisibleSteps));
    assert!(
        view.warnings
            .iter()
            .any(|w| w == "Sequence step limit reached; the callback passed to wrap is not shown."),
        "{:?}",
        view.warnings
    );
    assert!(view.participants.iter().all(|p| p.label != "thing"));
    let candidates = publish_with(&files, s::Language::Javascript, bindings(), true);
    assert_eq!(callbacks(&candidates).len(), 1);
}

/// The participant bound (#558): a callback whose interaction needs a 21st column is not
/// shown, and the view is truncated by participants.
#[test]
fn a_callback_never_adds_a_twenty_first_column() {
    let types: String = (0..19)
        .map(|n| format!("class T{n} {{\n  f{n}() {{}}\n}}\n"))
        .collect();
    let calling: String = (0..19).map(|n| format!("  t.f{n}();\n")).collect();
    let source = format!(
        "function run() {{\n{calling}  wrap(() => {{ other.thing(); }});\n}}\nfunction wrap(action) {{ return action(); }}\n{types}"
    );
    let mut bindings: Vec<Bind> = (0..19).map(|n| bind("run", n, &format!("f{n}"))).collect();
    bindings.push(bind("anonymous#0", 0, "thing"));
    let view = publish(
        &[("web/run.js", &source), ("web/other.js", JS_OTHER)],
        s::Language::Javascript,
        bindings,
    );
    let mut columns: Vec<_> = view
        .participants
        .iter()
        .map(|p| p.column.as_ref().unwrap().id.as_str())
        .collect();
    columns.sort_unstable();
    columns.dedup();
    assert_eq!(columns.len(), 20);
    assert!(callbacks(&view).is_empty());
    assert!(view.truncated_by.contains(&SequenceLimit::Participants));
    assert!(view.participants.iter().all(|p| p.label != "thing"));
}

#[test]
fn a_python_lambda_with_an_interaction_is_shown_under_its_call() {
    let view = publish(
        &[
            (
                "app/run.py",
                "def run():\n    wrap(lambda: other.thing())\n\ndef wrap(f):\n    return f()\n",
            ),
            (
                "app/other.py",
                "class Other:\n    def thing(self):\n        pass\n",
            ),
        ],
        s::Language::Python,
        vec![bind("run", 0, "wrap"), bind("anonymous#0", 0, "thing")],
    );
    // A Python lambda's boundary may leave the path, so the call it is passed to follows it
    // under a guard, not as its sibling: the block takes the boundary's place.
    let blocks = callbacks(&view);
    assert_eq!(blocks.len(), 1);
    assert_eq!(view.steps[0].kind, CALLBACK_KIND);
    assert!(blocks[0].label.starts_with("callback passed to wrap"));
    let targets: Vec<_> = all(&blocks[0].children)
        .into_iter()
        .filter_map(|step| target_name(&view, step))
        .collect();
    assert_eq!(targets, ["thing"]);
}

#[test]
fn a_java_lambda_with_an_interaction_is_shown_under_its_call() {
    let view = publish(
        &[
            (
                "src/Run.java",
                "class Run {\n  void run(Other other) { wrap(() -> other.thing()); }\n  void wrap(Runnable r) { r.run(); }\n}\n",
            ),
            ("src/Other.java", "class Other {\n  void thing() {}\n}\n"),
        ],
        s::Language::Java,
        vec![bind("run", 0, "wrap"), bind("anonymous#0", 0, "thing")],
    );
    let (_, targets) = shown_under(&view, "wrap");
    assert_eq!(targets, ["thing"]);
}
