fn test_pin(revision: u64) -> trellis::model::IndexPin {
    trellis::model::IndexPin {
        index_generation: uuid::Uuid::from_u128(0x00000000000040008000000000000001),
        index_revision: revision,
    }
}
use sha2::{Digest, Sha256};
use std::{
    cell::RefCell,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    behavior::{SequenceStep, SequenceView, build_sequence},
    indexer::{IndexOptions, index_workspace},
    model::*,
};
thread_local! { static FIXTURE_SOURCE: RefCell<String> = const { RefCell::new(String::new()) }; }

fn fixture(source: &str, name: &str) -> (Graph, SequenceView) {
    FIXTURE_SOURCE.with(|value| *value.borrow_mut() = source.into());
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fixture.rs"), source).unwrap();
    let graph = index_workspace(
        &IndexOptions::new(dir.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let seed = graph
        .nodes
        .iter()
        .find(|n| n.name == name)
        .unwrap_or_else(|| panic!("missing {name}: {:?}", graph.nodes));
    let file = &graph.files[0];
    let view = build_sequence(test_pin(7), seed, file, &graph.calls, false).unwrap();
    (graph, view)
}
fn flatten(steps: &[SequenceStep]) -> Vec<&SequenceStep> {
    let mut out = vec![];
    for s in steps {
        out.push(s);
        out.extend(flatten(&s.children));
        out.extend(flatten(&s.alternate));
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
fn nested_calls_keep_evaluation_order_and_evidence() {
    let (graph, view) = fixture(
        "fn run() { receiver().write(inner(first()), second()); }",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        ["receiver", "first", "inner", "second", "write"]
    );
    for s in flatten(&view.steps)
        .into_iter()
        .filter(|s| s.call_id.is_some())
    {
        let c = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == s.call_id.as_ref())
            .unwrap();
        assert_eq!(s.range, c.range);
        assert!(s.resolution.is_none() && s.target.is_none());
        assert_eq!(s.path, c.path);
    }
    assert_eq!(view.hidden_steps, 0);
}
#[test]
fn branches_and_return_guard_suffix() {
    let (_, view) = fixture(
        "fn run() { if check() { return failed(); } else { other(); } after(); }",
        "run",
    );
    let branch = view
        .steps
        .iter()
        .find(|s| s.label.starts_with("if "))
        .unwrap();
    assert_eq!(calls(&branch.children), ["failed"]);
    assert_eq!(calls(&branch.alternate), ["other"]);
    assert!(!view.steps.iter().any(|s| s.label == "after"));
    assert!(
        view.steps
            .iter()
            .any(|s| s.label.contains("continues normally") && calls(&s.children) == ["after"])
    );
}
#[test]
fn question_mark_guards_arguments_outer_call_and_suffix() {
    let (_, view) = fixture("fn run() { outer(first()?, second()); after(); }", "run");
    assert_eq!(calls(&view.steps), ["first", "second", "outer", "after"]);
    assert!(
        !view
            .steps
            .iter()
            .any(|s| ["outer", "second", "after"].contains(&s.label.as_str()))
    );
    assert!(
        flatten(&view.steps)
            .iter()
            .any(|s| s.kind == "return" && s.label.contains("residual"))
    );
}
#[test]
fn short_circuit_rhs_and_following_effects_are_guarded() {
    let (_, view) = fixture("fn run() { left() && right()?; after(); }", "run");
    let branch = view
        .steps
        .iter()
        .find(|s| s.label == "RHS only if left is true")
        .unwrap();
    assert_eq!(calls(&branch.children), ["right"]);
    assert!(!view.steps.iter().any(|s| s.label == "after"));
}
#[test]
fn match_guards_remain_inside_alternatives() {
    let (_, view) = fixture(
        "fn run() { match get() { Some(x) if check(x) => yes(), None => return no(), _ => other() }; after(); }",
        "run",
    );
    let branch = view
        .steps
        .iter()
        .find(|s| s.label.starts_with("match:"))
        .unwrap();
    assert_eq!(branch.alternate.len(), 3);
    assert_eq!(calls(&branch.alternate[0].children), ["check", "yes"]);
    assert_eq!(calls(&branch.alternate[1].children), ["no"]);
    assert!(!view.steps.iter().any(|s| s.label == "after"));
}
#[test]
fn loops_are_not_unrolled_and_return_hides_iteration_tail() {
    let (_, view) = fixture(
        "fn run() { for x in items() { if check(x) { return done(); } body(x); } after(); }",
        "run",
    );
    assert_eq!(
        calls(&view.steps),
        ["items", "check", "done", "body", "after"]
    );
    assert_eq!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.kind == "loop")
            .count(),
        1
    );
    assert!(!view.steps.iter().any(|s| s.label == "after"));
    let (_, view) = fixture(
        "fn run() { while check() { break; forbidden(); } after(); }",
        "run",
    );
    assert_eq!(calls(&view.steps), ["check", "after"]);
}
#[test]
fn async_await_and_opaque_bodies_are_honest() {
    let (_, view) = fixture(
        "async fn run() { let f = || closure_call(); let a = async { async_call(); }; opaque!(macro_call()); unsafe { unsafe_call(); } request().await; }",
        "run",
    );
    assert_eq!(calls(&view.steps), ["request"]);
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "await"));
    assert!(view.warnings.iter().any(|s| s.contains("when polled")));
    assert!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.kind == "boundary")
            .count()
            >= 4
    );
}
#[test]
fn assignment_evaluates_rhs_before_place_and_marks_write() {
    let (_, view) = fixture("fn run() { arr[index()] = value(); }", "run");
    assert_eq!(calls(&view.steps), ["value", "index"]);
    assert_eq!(view.steps.last().unwrap().kind, "effect");
}
#[test]
fn if_let_evaluates_value_and_keeps_body_guarded() {
    let (_, view) = fixture(
        "fn run() { if let Some(x) = lookup() { use_it(x); } else { missing(); } }",
        "run",
    );
    assert_eq!(calls(&view.steps), ["lookup", "use_it", "missing"]);
    assert_eq!(view.steps[0].label, "lookup");
    assert_eq!(calls(&view.steps[1].children), ["use_it"]);
}
#[test]
fn cache_is_sufficient_and_missing_calls_are_not_invented() {
    let (graph, view) = fixture("fn run() { work(); }", "run");
    // fixture's temporary source directory has already been removed.
    let cached = build_sequence(test_pin(8), &view.seed, &graph.files[0], &[], true).unwrap();
    assert!(calls(&cached.steps).is_empty());
    assert!(
        cached
            .warnings
            .iter()
            .any(|w| w.contains("no matching measured"))
    );
}
#[test]
fn bounds_are_explicit() {
    let source = format!("fn run() {{ {} }}", "work();".repeat(250));
    let (_, view) = fixture(&source, "run");
    assert!(view.truncated);
    assert!(flatten(&view.steps).len() <= 200);
}
#[test]
fn real_auth_function_is_supported_from_cached_source() {
    let (_, view) = fixture(include_str!("../../src/auth.rs"), "valid_token");
    assert!(!view.steps.is_empty());
    assert!(calls(&view.steps).iter().any(|s| s.contains("len")));
}

#[test]
fn outer_call_is_omitted_after_unconditional_argument_return() {
    let (_, view) = fixture(
        "fn run() { outer({ return early(); }, never()); after(); }",
        "run",
    );
    assert_eq!(calls(&view.steps), ["early"]);
}
#[test]
fn unsafe_macro_and_compound_assignment_guard_suffix() {
    for body in [
        "opaque!(return);",
        "unsafe { work(); }",
        "target()[index()] += value();",
    ] {
        let (_, view) = fixture(&format!("fn run() {{ {body} after(); }}"), "run");
        assert!(!view.steps.iter().any(|s| s.label == "after"));
        assert_eq!(calls(&view.steps), ["after"]);
    }
}
#[test]
fn method_fixture_covers_larger_guarded_workflow() {
    let (_, view) = fixture(
        r#"
    struct Worker;
    impl Worker {
      async fn run(&mut self, input: Input) -> Result<Value, Error> {
        if !valid(&input) { return reject(); }
        let key = make_key(&input);
        let result = self.client.send(encode(input)).await?;
        match result {
          Some(value) if allowed(&value) => { self.total = count(&value); save(value)?; }
          Some(_) => return denied(),
          None => fallback()?,
        }
        for item in pending() { if ready(&item) { record(item); } }
        finish(key)
      }
    }
    "#,
        "run",
    );
    let names = calls(&view.steps);
    assert_eq!(
        names,
        [
            "valid", "reject", "make_key", "encode", "send", "allowed", "count", "save", "denied",
            "fallback", "pending", "ready", "record", "finish"
        ]
    );
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "await"));
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "effect"));
    assert!(!view.truncated);
}
#[test]
fn depth_limit_is_explicit() {
    let source = format!(
        "fn run() {{ {}work(){}; }}",
        "(".repeat(110),
        ")".repeat(110)
    );
    let (_, view) = fixture(&source, "run");
    assert!(view.truncated);
    assert!(view.warnings.iter().any(|w| w.contains("Depth limit")));
}
/// #522: syntax nesting that adds no steps no longer exhausts the step-depth bound, so a
/// long function's deeply nested main loop keeps its calls.
#[test]
fn deep_syntax_without_nested_steps_keeps_its_calls() {
    let source = format!(
        "fn run() {{ {}work();{} }}",
        "{ ".repeat(30),
        " }".repeat(30)
    );
    let (_, view) = fixture(&source, "run");
    assert_eq!(calls(&view.steps), ["work"]);
    assert!(!view.truncated, "{:?}", view.warnings);
}
/// #522: the step tree itself stays within depth 24; a step at the bound that still nests
/// becomes one explicit boundary at its own source range.
#[test]
fn step_tree_depth_is_capped_with_an_explicit_boundary() {
    fn depth(steps: &[SequenceStep]) -> usize {
        steps
            .iter()
            .map(|s| 1 + depth(&s.children).max(depth(&s.alternate)))
            .max()
            .unwrap_or(0)
    }
    let source = format!(
        "fn run(a: bool) {{ {}work();{} }}",
        "if a { ".repeat(28),
        " }".repeat(28)
    );
    let (_, view) = fixture(&source, "run");
    assert!(view.truncated);
    assert_eq!(depth(&view.steps), 24);
    let boundary = flatten(&view.steps)
        .into_iter()
        .find(|s| s.label == "Depth limit: behavior omitted.")
        .unwrap();
    assert_eq!(boundary.kind, "boundary");
    assert!(boundary.children.is_empty() && boundary.call_id.is_none());
    assert!(calls(&view.steps).is_empty());
}
#[test]
fn participant_limit_preserves_call_evidence() {
    let source = format!(
        "fn run() {{ {} }}",
        (0..30).map(|i| format!("f{i}();")).collect::<String>()
    );
    let (graph, view) = fixture(&source, "run");
    // Matching names do not grant internal call targets.
    let view = build_sequence(
        test_pin(9),
        &view.seed,
        &graph.files[0],
        &graph.calls,
        false,
    )
    .unwrap();
    assert!(!view.truncated);
    assert!(view.participants.len() <= 20);
    assert_eq!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.call_id.is_some())
            .count(),
        30
    );
}
#[test]
fn show_all_does_not_filter_rust_calls() {
    let (graph, view) = fixture("fn run() { log(); debug(); }", "run");
    let all = build_sequence(test_pin(7), &view.seed, &graph.files[0], &graph.calls, true).unwrap();
    assert_eq!(view, all);
}

#[test]
fn all_return_match_and_unconditional_loop_return_omit_suffix() {
    for body in [
        "match get() { Some(_) => return yes(), None => return no() };",
        "loop { return done(); }",
        "loop { if check() { return yes(); } else { return no(); } }",
    ] {
        let (_, view) = fixture(&format!("fn run() {{ {body} forbidden(); }}"), "run");
        assert!(!calls(&view.steps).contains(&"forbidden"));
    }
    // A break is not a callable return. Keep the possible suffix.
    let (_, view) = fixture("fn run() { loop { break; } after(); }", "run");
    assert_eq!(calls(&view.steps), ["after"]);
}

fn ungroup(steps: Vec<SequenceStep>) -> Vec<SequenceStep> {
    steps
        .into_iter()
        .flat_map(|mut s| {
            s.children = ungroup(s.children);
            s.alternate = ungroup(s.alternate);
            if s.kind == "group" {
                s.children
            } else {
                vec![s]
            }
        })
        .collect()
}
fn assert_reversible(graph: &Graph, view: &SequenceView) {
    let all = build_sequence(test_pin(7), &view.seed, &graph.files[0], &graph.calls, true).unwrap();
    let mut restored = view.clone();
    restored.steps = ungroup(restored.steps);
    assert_eq!(restored, all);
    assert!(!flatten(&all.steps).iter().any(|s| s.kind == "group"));
}
#[test]
fn open_options_chain_and_inert_imports() {
    let (graph, view) = fixture(
        r#"
        fn run() -> Result<File, Error> {
            use std::fs::OpenOptions;
            use std::os::unix::fs::OpenOptionsExt;
            let file = OpenOptions::new().read(true).write(true).create(true)
                .truncate(false).mode(0o600).custom_flags(flags()).open(path)?;
            after();
        }
    "#,
        "run",
    );
    assert!(!view.truncated, "{:?}", view.warnings);
    let group = view.steps.iter().find(|s| s.kind == "group").unwrap();
    assert_eq!(
        calls(&group.children),
        [
            "OpenOptions::new",
            "read",
            "write",
            "create",
            "truncate",
            "mode",
            "flags",
            "custom_flags",
            "open"
        ]
    );
    assert!(
        group
            .label
            .starts_with("OpenOptions::new → read(true) → write(true)")
    );
    assert!(group.label.ends_with("(8 calls)"));
    assert!(group.call_id.is_none() && group.target.is_none() && group.resolution.is_none());
    assert_eq!(
        &graph.files[0].text[group.range.start_byte..group.range.end_byte],
        "OpenOptions::new().read(true).write(true).create(true)\n                .truncate(false).mode(0o600).custom_flags(flags()).open(path)"
    );
    assert!(!view.warnings.iter().any(|w| w.contains("use_declaration")));
    assert!(!view.steps.iter().any(|s| s.label == "after"));
    assert_reversible(&graph, &view);
}
#[test]
fn bare_use_does_not_guard_runtime_calls() {
    let (_, view) = fixture(
        "fn run() { use std::fs::File; use std::{io, path::*}; work(); }",
        "run",
    );
    assert_eq!(view.steps.len(), 1);
    assert_eq!(view.steps[0].label, "work");
}
#[test]
fn chain_arguments_and_unrelated_chains_keep_original_evidence() {
    let (graph, view) = fixture(
        "fn run() { A::new().a(first(), inner(second())).b(B::new().c(third()).d()); unrelated(); C::new().e().f(); }",
        "run",
    );
    assert_eq!(view.steps.iter().filter(|s| s.kind == "group").count(), 2);
    assert_eq!(
        calls(&view.steps),
        [
            "A::new",
            "first",
            "second",
            "inner",
            "a",
            "B::new",
            "third",
            "c",
            "d",
            "b",
            "unrelated",
            "C::new",
            "e",
            "f"
        ]
    );
    for s in flatten(&view.steps)
        .into_iter()
        .filter(|s| s.call_id.is_some())
    {
        let c = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == s.call_id.as_ref())
            .unwrap();
        assert_eq!(s.range, c.range);
        assert!(s.resolution.is_none() && s.target.is_none());
    }
    assert_reversible(&graph, &view);
}
#[test]
fn chain_control_and_opaque_arguments_are_not_hidden() {
    for source in [
        "async fn run() { A::new().a(check()?).b(); after(); }",
        "async fn run() { A::new().a(check().await).b(); after(); }",
        "fn run() { A::new().a(unsafe { check() }).b(); after(); }",
        "fn run() { A::new().a(opaque!()).b(); after(); }",
        "fn run() { A::new().a({ return check(); }).b(); after(); }",
        "fn run() { loop { A::new().a({ break; }).b(); } after(); }",
        "fn run() { A::new().a(if check() { yes() } else { no() }).b(); }",
    ] {
        let (graph, view) = fixture(source, "run");
        assert!(
            !flatten(&view.steps).iter().any(|s| s.kind == "group"),
            "{source}"
        );
        assert_reversible(&graph, &view);
    }
}
#[test]
fn open_options_chain_is_presentational() {
    let (graph, view) = fixture(
        include_str!("../fixtures/rust/open_options.rs"),
        "check_file_with_unlinked",
    );
    assert!(!view.warnings.iter().any(|w| w.contains("use_declaration")));
    assert!(
        flatten(&view.steps)
            .iter()
            .any(|s| s.kind == "group" && s.label.starts_with("std::fs::OpenOptions::new")),
        "{:?}",
        view.warnings
    );
    assert_reversible(&graph, &view);
}

#[test]
fn deep_opaque_macro_is_not_traversed_for_grouping() {
    let source = format!(
        "fn run() {{ opaque!({} Fake::new().a().b() {}); A::new().a().b(); }}",
        "(".repeat(512),
        ")".repeat(512)
    );
    let (graph, view) = fixture(&source, "run");
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "group"));
    assert_reversible(&graph, &view);
}

#[test]
fn grouping_work_budget_is_independent_and_keeps_measured_steps() {
    let source = format!(
        "fn run() {{ {} A::new().a().b(); }}",
        "use std::fs::File;".repeat(2000)
    );
    let (graph, view) = fixture(&source, "run");
    assert!(!view.truncated); // No measured behavior was lost.
    assert!(
        view.warnings
            .iter()
            .any(|w| w.contains("Chain grouping traversal truncated"))
    );
    let all = build_sequence(test_pin(7), &view.seed, &graph.files[0], &graph.calls, true).unwrap();
    assert_eq!(ungroup(view.steps), all.steps);
}

#[test]
fn grouping_depth_budget_is_independent_and_keeps_measured_steps() {
    let source = format!(
        "fn run() {{ use {}File{}; A::new().a().b(); }}",
        "std::{".repeat(40),
        "}".repeat(40)
    );
    let (graph, view) = fixture(&source, "run");
    assert!(!view.truncated);
    assert!(
        view.warnings
            .iter()
            .any(|w| w.contains("Chain grouping traversal truncated"))
    );
    let all = build_sequence(test_pin(7), &view.seed, &graph.files[0], &graph.calls, true).unwrap();
    assert_eq!(ungroup(view.steps), all.steps);
}

fn target_for<'a>(view: &'a SequenceView, label: &str) -> &'a trellis::behavior::Participant {
    let step = flatten(&view.steps)
        .into_iter()
        .find(|s| s.kind == "call" && s.label == label)
        .unwrap_or_else(|| panic!("missing call {label}"));
    assert!(step.target.is_none() && step.resolution.is_none());
    // Locate the source-only visual group without using a sequence target ID.
    let expression = FIXTURE_SOURCE.with(|source| {
        let source = source.borrow();
        let call = &source[step.range.start_byte..step.range.end_byte];
        if let Some((receiver, _)) = call.rsplit_once(&format!(".{label}")) {
            if receiver.contains(')') {
                "Chain results".to_owned()
            } else {
                receiver.to_owned()
            }
        } else if call.starts_with('(') || call.starts_with('[') {
            "Unresolved calls".to_owned()
        } else {
            label.to_owned()
        }
    });
    if expression == "Chain results" {
        return view
            .participants
            .iter()
            .find(|p| p.label == expression)
            .unwrap();
    }
    let kind = if expression == label {
        "unresolvedCallee"
    } else {
        "unresolvedReceiver"
    };
    let id = format!(
        "rust:source:{kind}:{}",
        hex::encode(Sha256::digest(expression.as_bytes()))
    );
    view.participants
        .iter()
        .find(|p| p.id == id)
        .or_else(|| view.participants.iter().find(|p| p.kind == "boundary"))
        .unwrap_or_else(|| panic!("missing terminal source group {label}: {expression}"))
}

#[test]
fn open_options_calls_have_source_hints_not_inferred_types() {
    let (graph, view) = fixture(
        include_str!("../fixtures/rust/open_options.rs"),
        "check_file_with_unlinked",
    );
    let file = target_for(&view, "metadata");
    assert_eq!((&*file.kind, &*file.label), ("unresolvedReceiver", "file"));
    let helper = target_for(&view, "check_file_metadata");
    assert_eq!(
        (&*helper.kind, &*helper.label),
        ("unresolvedCallee", "check_file_metadata")
    );
    let constructor = target_for(&view, "std::fs::OpenOptions::new");
    assert_eq!(constructor.kind, "unresolvedCallee");
    assert_eq!(constructor.label, "std::fs::OpenOptions::new");
    let chain = target_for(&view, "open");
    assert_eq!(
        (&*chain.kind, &*chain.label),
        ("unresolvedReceiver", "Chain results")
    );
    assert_eq!(chain, target_for(&view, "read"));
    assert!(
        !view
            .participants
            .iter()
            .any(|p| ["File", "OpenOptions"].contains(&p.label.as_str()))
    );
    for participant in [file, helper, constructor, chain] {
        assert!(
            participant
                .identification
                .contains("type and dispatch unresolved")
        );
        assert!(
            participant
                .identification
                .contains("not runtime object identity")
        );
    }
    for step in flatten(&view.steps)
        .into_iter()
        .filter(|s| s.call_id.is_some())
    {
        let measured = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == step.call_id.as_ref())
            .unwrap();
        assert_eq!(step.range, measured.range);
        assert!(step.resolution.is_none() && step.target.is_none());
        assert_eq!(step.path, measured.path);
    }
    assert_reversible(&graph, &view);
}

#[test]
fn imported_constructor_is_only_a_source_callee_hint() {
    let (graph, view) = fixture(
        "fn run() { use std::fs::OpenOptions as Options; Options::new().read(true).open(path); }",
        "run",
    );
    let constructor = target_for(&view, "Options::new");
    assert_eq!(
        (&*constructor.kind, &*constructor.label),
        ("unresolvedCallee", "Options::new")
    );
    assert!(
        constructor
            .identification
            .contains("Source expression only")
    );
    assert!(
        !view
            .participants
            .iter()
            .any(|p| p.label.contains("OpenOptions"))
    );
    assert_eq!(target_for(&view, "open").label, "Chain results");
    assert_reversible(&graph, &view);
}

#[test]
fn receiver_names_are_stable_source_groups_not_object_identity() {
    let source = "fn run() { file.read(); { let file = other; file.write(); } self.flush(); self.client.send(); settings.client.close(); file(); }";
    let (graph, view) = fixture(source, "run");
    let file = target_for(&view, "read");
    assert_eq!(file, target_for(&view, "write"));
    assert!(
        file.identification
            .contains("Repeated names are visual groups")
    );
    assert!(file.identification.contains("not runtime object identity"));
    assert_eq!(target_for(&view, "flush").label, "self");
    assert_eq!(target_for(&view, "send").label, "self.client");
    assert_eq!(target_for(&view, "close").label, "settings.client");
    let callee = target_for(&view, "file");
    assert_eq!(callee.kind, "unresolvedCallee");
    assert_ne!(callee.id, file.id);
    let rebuilt =
        build_sequence(test_pin(8), &view.seed, &graph.files[0], &graph.calls, true).unwrap();
    assert_eq!(target_for(&rebuilt, "read").id, file.id);
    let (_, shifted) = fixture(&format!("// shifted source\n{source}"), "run");
    assert_eq!(target_for(&shifted, "read").id, file.id);
}

#[test]
fn try_chain_results_stay_syntactic_and_guards_are_preserved() {
    let (graph, view) = fixture(
        "fn run() { get()?.read(); (other()?).write(); file.open()?.close(); after(); }",
        "run",
    );
    let chain = target_for(&view, "read");
    assert_eq!(chain.label, "Chain results");
    assert_eq!(chain, target_for(&view, "write"));
    assert_eq!(chain, target_for(&view, "close"));
    assert_eq!(target_for(&view, "open").label, "file");
    assert!(
        !view
            .steps
            .iter()
            .any(|s| ["read", "write", "close", "after"].contains(&s.label.as_str()))
    );
    assert_reversible(&graph, &view);
}

#[test]
fn dynamic_callees_keep_an_unresolved_boundary_lane() {
    let (_, view) = fixture(
        "fn run() { (callback)(); objects[index].read(); factory()(); }",
        "run",
    );
    for label in ["(callback)", "read", "factory()"] {
        let target = target_for(&view, label);
        assert_eq!(
            (&*target.kind, &*target.label),
            ("boundary", "Unresolved calls")
        );
    }
    assert_eq!(target_for(&view, "factory").kind, "unresolvedCallee");
}

#[test]
fn source_hint_limit_degrades_lanes_without_losing_calls_or_groups() {
    let source = format!(
        "fn run() {{ {} A::new().a().b(); }}",
        (0..30)
            .map(|i| format!("receiver{i}.work{i}();"))
            .collect::<String>()
    );
    let (graph, view) = fixture(&source, "run");
    assert_eq!(view.participants.len(), 20);
    assert!(!view.truncated);
    assert!(
        view.warnings
            .iter()
            .any(|w| w.contains("Source-hint participant limit"))
    );
    assert_eq!(calls(&view.steps).len(), 33);
    assert!(flatten(&view.steps).iter().any(|s| s.kind == "group"));
    for step in flatten(&view.steps)
        .into_iter()
        .filter(|s| s.call_id.is_some())
    {
        assert_eq!(step.kind, "call");
        assert!(step.target.is_none() && step.resolution.is_none());
        let measured = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == step.call_id.as_ref())
            .unwrap();
        assert_eq!(step.range, measured.range);
        assert!(step.resolution.is_none() && step.target.is_none());
    }
    assert_eq!(target_for(&view, "work29").kind, "boundary");
    assert_reversible(&graph, &view);
}

#[test]
fn source_hint_labels_are_bounded_without_identity_collisions() {
    let prefix = "a".repeat(200);
    let (_, view) = fixture(
        &format!("fn run() {{ {prefix}x.first(); {prefix}y.second(); }}"),
        "run",
    );
    let first = target_for(&view, "first");
    let second = target_for(&view, "second");
    assert_eq!(first.label.chars().count(), 181);
    assert_eq!(first.label, second.label);
    assert_ne!(first.id, second.id);
    assert!(first.id.len() < 100);
}

#[test]
fn confirmed_internal_targets_are_not_replaced_by_source_hints() {
    let (graph, view) = fixture("fn run() { file.read(); }", "run");
    let measured = build_sequence(
        test_pin(7),
        &view.seed,
        &graph.files[0],
        &graph.calls,
        false,
    )
    .unwrap();
    let target = target_for(&measured, "read");
    assert_ne!(target.id, "measured:symbol");
    assert_ne!(target.kind, "internal");
    assert!(measured.steps[0].resolution.is_none() && measured.steps[0].target.is_none());
}
