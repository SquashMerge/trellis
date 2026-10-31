fn test_pin(revision: u64) -> trellis::model::IndexPin {
    trellis::model::IndexPin {
        index_generation: uuid::Uuid::from_u128(0x00000000000040008000000000000001),
        index_revision: revision,
    }
}
use std::sync::{Arc, atomic::AtomicBool};
use trellis::{
    behavior::{SequenceStep, SequenceView, build_sequence},
    indexer::{IndexOptions, index_workspace},
    model::*,
};
fn fixture(source: &str, name: &str) -> (Graph, SequenceView) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("Fixture.java"), source).unwrap();
    let graph = index_workspace(
        &IndexOptions::new(dir.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let seed = graph
        .nodes
        .iter()
        .filter(|n| n.name == name && matches!(n.kind, SymbolKind::Function | SymbolKind::Method))
        .min_by_key(|n| n.range.start_byte)
        .unwrap_or_else(|| panic!("missing {name}: {:?}", graph.nodes));
    let view = build_sequence(test_pin(7), seed, &graph.files[0], &graph.calls, false).unwrap();
    (graph, view)
}
fn flatten(steps: &[SequenceStep]) -> Vec<&SequenceStep> {
    let mut out = vec![];
    for step in steps {
        out.push(step);
        out.extend(flatten(&step.children));
        out.extend(flatten(&step.alternate));
    }
    out
}
fn calls(steps: &[SequenceStep]) -> Vec<&str> {
    flatten(steps)
        .into_iter()
        .filter(|s| s.kind == "call")
        .map(|s| s.label.as_str())
        .collect()
}
#[test]
fn receiver_arguments_constructor_assignment_order_and_measured_evidence() {
    let (graph, view) = fixture(
        "class Fixture { void run() { receiver().write(inner(first()), second()); array()[index()] = value(); target().field += rhs(); new Box(make()); outer().new Inner(arg()); } }",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "receiver",
            "first",
            "inner",
            "second",
            "write",
            "array",
            "index",
            "value",
            "target",
            "rhs",
            "make",
            "new Box",
            "outer",
            "arg",
            "new Inner"
        ]
    );
    for step in flatten(&view.steps)
        .into_iter()
        .filter(|s| s.call_id.is_some())
    {
        let c = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == step.call_id.as_ref())
            .unwrap();
        assert_eq!(step.range, c.range);
        assert_eq!(step.path, c.path);
        assert!(step.resolution.is_none() && step.target.is_none());
    }
    assert_eq!(view.hidden_steps, 0);
    assert!(
        view.participants
            .iter()
            .skip(1)
            .all(|p| p.kind != "internal" && p.identification.contains("unresolved"))
    );
    assert_eq!(
        view,
        build_sequence(test_pin(7), &view.seed, &graph.files[0], &graph.calls, true).unwrap()
    );
}
#[test]
fn branches_returns_throws_and_short_circuit_guard_suffixes() {
    let (_, view) = fixture(
        "class Fixture { Object run() { if (check() && ready()) { return done(); } else { other(); } after(); throw error(); unreachable(); } }",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        ["check", "ready", "done", "other", "after", "error"]
    );
    let short = view
        .steps
        .iter()
        .find(|s| s.label == "RHS only if left is true")
        .unwrap();
    assert_eq!(calls(&short.children), ["ready"]);
    let branch = view
        .steps
        .iter()
        .find(|s| s.label.starts_with("if "))
        .unwrap();
    assert_eq!(calls(&branch.children), ["done"]);
    assert_eq!(calls(&branch.alternate), ["other"]);
    assert!(!view.steps.iter().any(|s| s.label == "after"));
    assert!(view.steps.iter().any(
        |s| s.label.contains("continues normally") && calls(&s.children) == ["after", "error"]
    ));
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "throw"));
    let (_, both) = fixture(
        "class Fixture { void run() { if (check()) return; else throw error(); gone(); } }",
        "run",
    );
    assert_eq!(calls(&both.steps), ["check", "error"]);
}
#[test]
fn ternary_while_and_break_are_structured_not_flattened() {
    let (_, view) = fixture(
        "class Fixture { void run() { use(check() ? yes() : no()); while (next()) { body(); if (stop()) break; tail(); } after(); } }",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "check", "yes", "no", "use", "next", "body", "stop", "tail", "after"
        ]
    );
    let branch = view
        .steps
        .iter()
        .find(|s| s.label.starts_with("if "))
        .unwrap();
    assert_eq!(calls(&branch.children), ["yes"]);
    assert_eq!(calls(&branch.alternate), ["no"]);
    let loop_step = view.steps.iter().find(|s| s.kind == "loop").unwrap();
    assert_eq!(calls(&loop_step.children), ["next", "body", "stop", "tail"]);
    assert!(!view.steps.iter().any(|s| s.label == "after"));
}
#[test]
fn nested_lambda_and_anonymous_bodies_never_execute_at_construction() {
    let (graph, view) = fixture(
        "class Fixture { void run() { use(x -> inside(x)); new Base(arg()) { { initOnly(); } void nested() { nestedOnly(); } }; class Local { void other() { otherOnly(); } } after(); } }",
        "run",
    );
    assert_eq!(calls(&view.steps), ["use", "arg", "new Base", "after"]);
    assert!(!calls(&view.steps).contains(&"inside"));
    assert!(!calls(&view.steps).contains(&"initOnly"));
    assert!(!calls(&view.steps).contains(&"nestedOnly"));
    assert!(graph.calls.iter().all(|call| {
        call.caller == view.seed.id
            || !view
                .steps
                .iter()
                .any(|step| step.call_id.as_deref() == Some(&call.id))
    }));
}
#[test]
fn signatures_and_annotation_defaults_have_no_invocation_body() {
    for (source, name) in [
        ("interface Work { void run(); }", "run"),
        ("abstract class Work { abstract void run(); }", "run"),
        ("class Work { native void run(); }", "run"),
        (
            "@interface Mark { String value() default \"value\"; }",
            "value",
        ),
    ] {
        let (_, view) = fixture(source, name);
        assert!(calls(&view.steps).is_empty());
        assert!(
            view.steps
                .iter()
                .any(|s| s.kind == "boundary" && s.label.contains("No callable body"))
        );
    }
}
#[test]
fn explicit_and_compact_constructors_are_navigable() {
    let (_, view) = fixture(
        "class Fixture { Fixture() { this(first()); after(); } Fixture(Object x) { super(); } }",
        "Fixture",
    );
    assert_eq!(calls(&view.steps), ["first", "this", "after"]);
    let (_, view) = fixture("record Pair(String x) { Pair { validate(x); } }", "Pair");
    assert_eq!(calls(&view.steps), ["validate"]);
}
#[test]
fn unsupported_control_and_initialization_forms_are_opaque() {
    let cases = [
        "try { hidden(); } catch (Exception e) { caught(); } finally { cleanup(); }",
        "try (var x = resource()) { hidden(); }",
        "switch (value()) { case 1 -> hidden(); default -> other(); }",
        "synchronized (lock()) { hidden(); }",
        "for (init(); check(); update()) { if (stop()) continue; hidden(); }",
        "do { hidden(); } while (check());",
        "assert check() : message();",
        "Object r = receiver()::method;",
        "Object x = new Object[size()];",
        "boolean matched = value() instanceof Point(var x, var y);",
    ];
    for body in cases {
        let (_, view) = fixture(
            &format!("class Fixture {{ void run() {{ {body} after(); }} }}"),
            "run",
        );
        assert!(
            !calls(&view.steps).iter().any(|c| [
                "hidden", "caught", "cleanup", "resource", "value", "other", "lock", "init",
                "check", "update", "items", "message", "receiver", "size"
            ]
            .contains(c)),
            "{body}: {:?}",
            view.steps
        );
        assert!(
            flatten(&view.steps).iter().any(|s| s.kind == "boundary"),
            "{body}"
        );
        assert!(!view.steps.iter().any(|s| s.label == "after"), "{body}");
    }
}
#[test]
fn malformed_body_is_not_a_false_linear_trace() {
    let (_, view) = fixture(
        "class Fixture { void run() { okay(); if ( { broken(); } } }",
        "run",
    );
    assert!(calls(&view.steps).is_empty());
    assert!(view.warnings.iter().any(|w| w.contains("Malformed")));
}
#[test]
fn depth_steps_participants_and_wide_inert_work_are_bounded() {
    let (_, many) = fixture(
        &format!(
            "class Fixture {{ void run() {{ {} }} }}",
            (0..250)
                .map(|i| format!("callee{i}();"))
                .collect::<String>()
        ),
        "run",
    );
    assert!(many.truncated);
    assert!(flatten(&many.steps).len() <= 200);
    assert!(many.participants.len() <= 20);
    let deep = format!(
        "class Fixture {{ void run() {{ {}inner(){}; }} }}",
        "outer(".repeat(40),
        ")".repeat(40)
    );
    let (_, view) = fixture(&deep, "run");
    assert!(view.truncated);
    let (_, wide) = fixture(
        &format!(
            "class Fixture {{ void run() {{ {} after(); }} }}",
            (0..11_000)
                .map(|i| format!("int x{i};"))
                .collect::<String>()
        ),
        "run",
    );
    assert!(wide.truncated);
    assert!(!calls(&wide.steps).contains(&"after"));
}

#[test]
fn for_loops_initialize_once_and_guard_body_updates_and_suffix() {
    let (_, view) = fixture(
        "class Fixture { void run() { for (init(), init2(); check(); update(), update2()) { if (stop()) return; body(); } after(); } }",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "init", "init2", "check", "stop", "body", "update", "update2", "after"
        ]
    );
    assert_eq!(view.steps[0].label, "init");
    assert_eq!(view.steps[1].label, "init2");
    let loop_step = view.steps.iter().find(|s| s.kind == "loop").unwrap();
    assert_eq!(
        calls(&loop_step.children),
        ["check", "stop", "body", "update", "update2"]
    );
    assert!(!view.steps.iter().any(|s| s.label == "after"));
    let (_, view) = fixture(
        "class Fixture { void run() { for (var item : items()) { body(item); } after(); } }",
        "run",
    );
    assert_eq!(calls(&view.steps), ["items", "body", "after"]);
    assert_eq!(view.steps[0].label, "items");
    assert_eq!(calls(&view.steps[1].children), ["body"]);
    let (_, view) = fixture(
        "class Fixture { void run() { for (init(); check(); update()) { return; never(); } after(); } }",
        "run",
    );
    assert_eq!(calls(&view.steps), ["init", "check", "after"]);
}

#[test]
fn exhausted_argument_visit_budget_never_implies_enclosing_invocation() {
    let mut expression = "1".to_string();
    for _ in 0..4 {
        expression = format!("({expression}+{expression})");
    }
    let arguments = std::iter::repeat_n(expression, 220)
        .collect::<Vec<_>>()
        .join(",");
    let (_, view) = fixture(
        &format!(
            "class Fixture {{ void run() {{ consume({arguments}, dangerous()); after(); }} }}"
        ),
        "run",
    );
    assert!(view.truncated);
    assert!(
        flatten(&view.steps)
            .iter()
            .any(|s| s.kind == "boundary" && s.label.contains("Traversal budget exhausted"))
    );
    assert!(calls(&view.steps).is_empty(), "{:?}", view.steps);
    assert!(!view.warnings.iter().any(|w| w.contains("unreachable")));
}
