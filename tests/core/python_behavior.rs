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
    std::fs::write(dir.path().join("fixture.py"), source).unwrap();
    let graph = index_workspace(
        &IndexOptions::new(dir.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(graph.stats.parse_error_files, 0, "{:?}", graph.diagnostics);
    let seed = graph.nodes.iter().find(|n| n.name == name).unwrap();
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
fn evaluation_order_source_evidence_and_no_constructor_guess() {
    let (graph, view) = fixture(
        "def run():\n    receiver().write(inner(first()), second(), named=third())\n    Thing()\n    café()\n",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "receiver", "first", "inner", "second", "third", "write", "Thing", "café"
        ]
    );
    for step in flatten(&view.steps)
        .into_iter()
        .filter(|s| s.call_id.is_some())
    {
        let call = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == step.call_id.as_ref())
            .unwrap();
        assert_eq!(step.range, call.range);
        assert_eq!(step.path, call.path);
        assert!(step.resolution.is_none() && step.target.is_none());
    }
    assert!(
        view.participants
            .iter()
            .any(|p| p.label == "Thing" && p.kind == "unresolvedCallee")
    );
    assert!(!view.participants.iter().any(|p| p.kind == "internal"));
    assert_eq!(
        view,
        build_sequence(test_pin(7), &view.seed, &graph.files[0], &graph.calls, true).unwrap()
    );
    let missing = build_sequence(test_pin(8), &view.seed, &graph.files[0], &[], true).unwrap();
    assert!(calls(&missing.steps).is_empty());
    assert!(
        missing
            .warnings
            .iter()
            .any(|w| w.contains("no matching measured"))
    );
}
#[test]
fn branches_elif_return_and_raise_guard_suffix() {
    let (_, view) = fixture(
        "def run():\n    if check():\n        return done()\n    elif other():\n        raise failure() from cause()\n    else:\n        fallback()\n    after()\n",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "check", "done", "other", "failure", "cause", "fallback", "after"
        ]
    );
    let branch = view
        .steps
        .iter()
        .find(|s| s.label.starts_with("if "))
        .unwrap();
    assert_eq!(calls(&branch.children), ["done"]);
    assert_eq!(
        calls(&branch.alternate),
        ["other", "failure", "cause", "fallback"]
    );
    assert!(!view.steps.iter().any(|s| s.label == "after"));
    assert!(
        view.steps
            .iter()
            .any(|s| s.label.contains("continues normally") && calls(&s.children) == ["after"])
    );
    let (_, view) = fixture(
        "def run():\n    if check():\n        return yes()\n    else:\n        return no()\n    forbidden()\n",
        "run",
    );
    assert!(!calls(&view.steps).contains(&"forbidden"));
}
#[test]
fn short_circuit_conditional_expression_and_assignment_order() {
    let (_, view) = fixture(
        "def run():\n    left() and right()\n    a() or b()\n    result = yes() if check() else no()\n    target()[index()] = value()\n",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "left", "right", "a", "b", "check", "yes", "no", "value", "target", "index"
        ]
    );
    assert!(
        !view
            .steps
            .iter()
            .any(|s| s.label == "right" || s.label == "b")
    );
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "effect"));
}
#[test]
fn loops_keep_conditions_inside_loop_and_transfers_stop_path() {
    let (_, view) = fixture(
        "def run():\n    for x in items():\n        if check(x):\n            return done()\n        work(x)\n    while ready():\n        break\n        forbidden()\n    after()\n",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        ["items", "check", "done", "work", "ready", "after"]
    );
    assert_eq!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.kind == "loop")
            .count(),
        2
    );
    assert!(
        !view
            .steps
            .iter()
            .any(|s| s.label == "ready" || s.label == "after")
    );
}
#[test]
fn nested_definitions_class_execution_and_lambda_invocation_are_separate() {
    let (graph, view) = fixture(
        "async def run(arg=outer_default()):\n    @decorate()\n    def nested(x=default()):\n        nested_call()\n    class Local(base()):\n        class_call()\n        def method(self):\n            method_call()\n    callback = lambda x=lambda_default(): lambda_call(x)\n    await request()\n",
        "run",
    );
    assert_eq!(calls(&view.steps), ["request"]);
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "await"));
    assert!(
        view.warnings
            .iter()
            .any(|w| w.contains("Definition boundary"))
    );
    assert!(!calls(&view.steps).contains(&"lambda_call"));
    assert!(graph.calls.iter().all(|call| {
        !view
            .steps
            .iter()
            .any(|step| step.call_id.as_ref() == Some(&call.id) && call.path != view.seed.path)
    }));
}
#[test]
fn unsupported_forms_are_boundaries_not_unconditional_calls() {
    for body in [
        "    values = [work(x) for x in items() if check(x)]\n",
        "    with context():\n        work()\n",
        "    match subject():\n        case 1:\n            work()\n",
        "    try:\n        work()\n    except Error:\n        recover()\n    else:\n        succeeded()\n    finally:\n        cleanup()\n",
        "    for x in items():\n        work()\n    else:\n        done()\n",
        "    async for x in items():\n        work()\n",
        "    f(key=first(), *second())\n",
        "    a() < b() < c()\n",
        "    yield from source()\n",
        "    target().x = other().y = value()\n",
    ] {
        let (_, view) = fixture(&format!("async def run():\n{body}    after()\n"), "run");
        assert_eq!(calls(&view.steps), ["after"], "{body}");
        assert!(flatten(&view.steps).iter().any(|s| s.kind == "boundary"));
        assert!(!view.steps.iter().any(|s| s.label == "after"), "{body}");
    }
}
#[test]
fn step_participant_depth_and_width_bounds_are_explicit() {
    let source = format!("def run():\n{}", "    work()\n".repeat(250));
    let (_, view) = fixture(&source, "run");
    assert!(view.truncated);
    assert!(flatten(&view.steps).len() <= 200);
    let source = format!(
        "def run():\n{}",
        (0..50)
            .map(|i| format!("    receiver{i}.work()\n"))
            .collect::<String>()
    );
    let (_, view) = fixture(&source, "run");
    assert!(view.participants.len() <= 20);
    assert_eq!(calls(&view.steps).len(), 50);
    let source = format!(
        "def run():\n    {}work(){}\n",
        "(".repeat(40),
        ")".repeat(40)
    );
    let (_, view) = fixture(&source, "run");
    assert!(view.truncated);
    let source = format!("def run():\n{}", "    pass\n".repeat(5000));
    let (_, view) = fixture(&source, "run");
    assert!(view.truncated);
    assert!(calls(&view.steps).is_empty());
}

#[test]
fn loop_targets_evaluate_addresses_without_getter_claims() {
    let (_, view) = fixture(
        "def run():\n    for receiver().x in values():\n        work()\n    for target()[index()] in other():\n        finish()\n",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "values", "receiver", "work", "other", "target", "index", "finish"
        ]
    );
    assert!(
        !flatten(&view.steps)
            .iter()
            .any(|s| s.label.contains("Attribute lookup") || s.label.contains("__getitem__"))
    );
    assert_eq!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.label.starts_with("iteration target write"))
            .count(),
        2
    );
}

#[test]
fn visit_cutoff_guards_enclosing_invocation_after_omitted_arguments() {
    fn balanced(depth: usize) -> String {
        if depth == 0 {
            return "1".into();
        }
        let child = balanced(depth - 1);
        format!("({child} + {child})")
    }
    let arguments = std::iter::repeat_n(balanced(4), 220)
        .collect::<Vec<_>>()
        .join(", ");
    let (_, view) = fixture(
        &format!("def run():\n    consume({arguments}, dangerous())\n    after()\n"),
        "run",
    );
    assert!(view.truncated);
    assert!(flatten(&view.steps).len() <= 200);
    assert!(
        flatten(&view.steps)
            .iter()
            .any(|s| s.kind == "boundary" && s.label.contains("Traversal limit"))
    );
    assert!(
        !view
            .steps
            .iter()
            .any(|s| s.kind == "call" && s.label == "consume")
    );
    assert!(!calls(&view.steps).contains(&"dangerous"));
    // If retained at all, the outer invocation must follow an explicit continuation guard.
    assert!(
        !calls(&view.steps).contains(&"consume")
            || view
                .steps
                .iter()
                .any(|s| s.kind == "branch" && calls(&s.children).contains(&"consume"))
    );
}

#[test]
fn local_annotations_never_execute_and_valued_targets_keep_assignment_order() {
    let (graph, view) = fixture(
        "def run():\n    x: annotation_call() = rhs()\n    obj().field: annotation_call() = other_rhs()\n    target()[index()]: annotation_call() = final_rhs()\n    local: annotation_call()\n    after()\n",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "rhs",
            "other_rhs",
            "obj",
            "final_rhs",
            "target",
            "index",
            "after"
        ]
    );
    assert!(
        graph
            .calls
            .iter()
            .any(|c| c.callee_text.as_deref() == Some("annotation_call"))
    );
    assert!(!calls(&view.steps).contains(&"annotation_call"));
    assert_eq!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.kind == "effect")
            .count(),
        3
    );
    assert!(!flatten(&view.steps).iter().any(|s| s.kind == "boundary"));
    assert!(!view.steps.iter().any(|s| s.kind == "branch"));
    let (_, boundary) = fixture(
        "def run():\n    obj().field: annotation_call()\n    after()\n",
        "run",
    );
    assert!(
        flatten(&boundary.steps)
            .iter()
            .any(|s| s.label.contains("Annotation-only target boundary"))
    );
    assert!(!calls(&boundary.steps).contains(&"annotation_call"));
}

#[test]
fn class_annotations_remain_inside_class_execution_boundary() {
    let (graph, view) = fixture(
        "def run():\n    class Local:\n        field: class_annotation() = class_rhs()\n    def nested():\n        value: nested_annotation() = nested_rhs()\n    after()\n",
        "run",
    );
    assert_eq!(calls(&view.steps), ["after"]);
    assert!(
        flatten(&view.steps)
            .iter()
            .any(|s| s.label.contains("Class definition boundary"))
    );
    let nested = graph.nodes.iter().find(|n| n.name == "nested").unwrap();
    let selected =
        build_sequence(test_pin(8), nested, &graph.files[0], &graph.calls, true).unwrap();
    assert_eq!(calls(&selected.steps), ["nested_rhs"]);
}

#[test]
fn future_annotations_does_not_change_local_assignment_execution() {
    for prefix in ["", "from __future__ import annotations\n"] {
        let (graph, view) = fixture(
            &format!(
                "{prefix}def run():\n    x: annotation_call() = rhs()\n    obj().field: annotation_call() = other()\n    target()[key()]: annotation_call() = last()\n"
            ),
            "run",
        );
        assert_eq!(
            calls(&view.steps),
            ["rhs", "other", "obj", "last", "target", "key"]
        );
        assert!(
            graph
                .calls
                .iter()
                .any(|c| c.callee_text.as_deref() == Some("annotation_call"))
        );
        assert!(!flatten(&view.steps).iter().any(|s| s.kind == "boundary"));
    }
}

#[test]
fn postponed_nested_async_factory_is_straight_and_body_stays_deferred() {
    let source = "\"\"\"Module docs.\"\"\"\n# header comment\nfrom __future__ import annotations\ndef create_workflow():\n    agent = make_agent()\n    async def _executor(step_input: annotation_call()) -> ReturnType[other_annotation()]:\n        return await callback(step_input)\n    return Workflow(steps=[Step(executor=_executor)])\n";
    let (graph, view) = fixture(source, "create_workflow");
    assert_eq!(calls(&view.steps), ["make_agent", "Step", "Workflow"]);
    assert!(
        !flatten(&view.steps)
            .iter()
            .any(|s| s.kind == "branch" || s.kind == "boundary")
    );
    let binding = view.steps.iter().find(|s| s.kind == "effect").unwrap();
    assert_eq!(binding.label, "bind name: agent");
    assert_eq!(
        &source[binding.range.start_byte..binding.range.end_byte],
        "agent = make_agent()"
    );
    let definition = view.steps.iter().find(|s| s.kind == "definition").unwrap();
    assert_eq!(definition.label, "define _executor · async body deferred");
    assert!(definition.children.is_empty());
    assert!(definition.alternate.is_empty());
    let callback = graph.nodes.iter().find(|n| n.name == "_executor").unwrap();
    let selected =
        build_sequence(test_pin(8), callback, &graph.files[0], &graph.calls, false).unwrap();
    assert_eq!(calls(&selected.steps), ["callback"]);
    assert!(flatten(&selected.steps).iter().any(|s| s.kind == "await"));
    assert!(graph.calls.iter().any(|c| {
        c.callee_text
            .as_deref()
            .is_some_and(|name| name.contains("annotation"))
    }));
    assert!(
        !calls(&view.steps)
            .iter()
            .any(|name| name.contains("annotation"))
    );
    assert!(
        !calls(&selected.steps)
            .iter()
            .any(|name| name.contains("annotation"))
    );
}

#[test]
fn plain_nested_definition_binds_without_executing_its_body() {
    let (_, view) = fixture(
        "def run():\n    def helper(a, /, *args, flag, **kwargs):\n        raise body_failure()\n    after()\n",
        "run",
    );
    assert_eq!(calls(&view.steps), ["after"]);
    assert_eq!(view.steps[0].kind, "definition");
    assert_eq!(view.steps[0].label, "define helper · body deferred");
    assert_eq!(view.steps[1].label, "after");
    assert!(!flatten(&view.steps).iter().any(|s| s.kind == "branch"));
}

#[test]
fn eager_or_unsupported_definition_headers_keep_continuation_guards() {
    for (prefix, definition) in [
        ("", "    def helper(x: Input) -> Output:\n        body()\n"),
        ("", "    def helper(x: annotation()):\n        body()\n"),
        (
            "from __future__ import annotations\n",
            "    def helper(x=default()):\n        body()\n",
        ),
        (
            "from __future__ import annotations\n",
            "    def helper(x: Input=default()):\n        body()\n",
        ),
        (
            "from __future__ import annotations\n",
            "    @decorate\n    def helper():\n        body()\n",
        ),
        (
            "from __future__ import annotations\n",
            "    @decorate()\n    def helper():\n        body()\n",
        ),
        (
            "from __future__ import annotations\n",
            "    def helper[T](x: T):\n        body()\n",
        ),
        (
            "from __future__ import annotations\n",
            "    callback = lambda: body()\n",
        ),
        (
            "from __future__ import annotations\n",
            "    class Local:\n        body()\n",
        ),
    ] {
        let (_, view) = fixture(
            &format!("{prefix}def run():\n{definition}    after()\n"),
            "run",
        );
        assert_eq!(calls(&view.steps), ["after"], "{definition}");
        assert!(
            !view
                .steps
                .iter()
                .any(|s| s.kind == "call" && s.label == "after"),
            "{definition}"
        );
        assert!(
            flatten(&view.steps).iter().any(|s| s.kind == "boundary"),
            "{definition}"
        );
    }
}

#[test]
fn only_proven_module_future_header_defers_annotations() {
    for prefix in [
        "# from __future__ import annotations\n",
        "\"from __future__ import annotations\"\n",
        "def other():\n    from __future__ import annotations\n",
        "ordinary()\nfrom __future__ import annotations\n",
        "f\"not a docstring\"\nfrom __future__ import annotations\n",
        "b\"not a docstring\"\nfrom __future__ import annotations\n",
        "\"docstring\"\n\"second string is a statement\"\nfrom __future__ import annotations\n",
        "from package import annotations\n",
        "from __future__ import made_up_feature, annotations\n",
    ] {
        let (_, view) = fixture(
            &format!(
                "{prefix}def run():\n    def helper(x: annotation_call()):\n        body()\n    after()\n"
            ),
            "run",
        );
        assert_eq!(calls(&view.steps), ["after"], "{prefix}");
        assert!(
            !view.steps.iter().any(|s| s.kind == "definition"),
            "{prefix}"
        );
        assert!(!view.steps.iter().any(|s| s.kind == "call"), "{prefix}");
    }
    let (_, view) = fixture(
        "# comment\nfrom __future__ import division\nfrom __future__ import (annotations,)\ndef run():\n    def helper(*args: annotation_call(), **kwargs: Other):\n        body()\n    after()\n",
        "run",
    );
    assert_eq!(view.steps[0].kind, "definition");
    assert_eq!(view.steps[1].label, "after");
}

#[test]
fn identifier_binding_does_not_relabel_complex_writes() {
    let (_, view) = fixture(
        "def run():\n    agent = make_agent()\n    target().field = value()\n    target()[index()] = other()\n    a, b = pair()\n    after()\n",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        [
            "make_agent",
            "value",
            "target",
            "other",
            "target",
            "index",
            "pair",
            "after"
        ]
    );
    let flat = flatten(&view.steps);
    assert_eq!(
        flat.iter()
            .filter(|s| s.label == "bind name: agent")
            .count(),
        1
    );
    assert_eq!(
        flat.iter()
            .filter(|s| s.label.starts_with("write:") && s.label.contains("setters/unpacking"))
            .count(),
        3
    );
    assert!(
        flat.iter()
            .any(|s| s.label.contains("Assignment target boundary"))
    );
    assert!(!view.steps.iter().any(|s| s.label == "after"));
}

#[test]
fn definition_header_shortcuts_are_bounded() {
    let parameters = (0..5000)
        .map(|i| format!("p{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let (_, view) = fixture(
        &format!("def run():\n    def helper({parameters}):\n        body()\n    after()\n"),
        "run",
    );
    assert!(view.truncated);
    assert!(!flatten(&view.steps).iter().any(|s| s.kind == "definition"));
    assert!(flatten(&view.steps).len() <= 200);
    let comments = "# header comment\n".repeat(5000);
    let (_, view) = fixture(
        &format!(
            "{comments}from __future__ import annotations\ndef run():\n    def helper(x: Input):\n        body()\n    after()\n"
        ),
        "run",
    );
    assert!(!flatten(&view.steps).iter().any(|s| s.kind == "definition"));
}
