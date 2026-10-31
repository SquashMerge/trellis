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
    std::fs::write(dir.path().join("fixture.js"), source).unwrap();
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
fn atomic_write_retains_static_paths_and_exact_call_evidence() {
    let (graph, view) = fixture(
        r#"
async function atomicWrite(path, value) {
  if (!valid(path)) throw new Error("invalid");
  const tmp = tempName(path);
  try {
    await fs.writeFile(tmp, encode(value));
    await fs.rename(tmp, path);
    return path;
  } catch (err) {
    await fs.unlink(tmp);
    throw err;
  } finally {
    release(path);
  }
}
"#,
        "atomicWrite",
    );
    let all = flatten(&view.steps);
    assert!(all.iter().any(|s| s.kind == "try"));
    assert!(all.iter().any(|s| s.kind == "await"));
    assert!(all.iter().any(|s| s.label.contains("catch: only if")));
    assert!(all.iter().any(|s| s.label.contains("finally: on both")));
    let names = calls(&view.steps);
    assert!(
        names.iter().position(|s| *s == "encode").unwrap()
            < names.iter().position(|s| *s == "fs.writeFile").unwrap()
    );
    for step in all.iter().filter(|s| s.call_id.is_some()) {
        let original = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == step.call_id.as_ref())
            .unwrap();
        assert_eq!(step.range, original.range);
        assert_eq!(step.path, original.path);
        assert!(step.resolution.is_none() && step.target.is_none());
        let source = &graph.files[0].text[step.range.start_byte..step.range.end_byte];
        assert!(!source.is_empty());
    }
    let wire = serde_json::to_value(&view).unwrap();
    assert_eq!(wire["revision"], serde_json::json!(test_pin(7)));
    assert!(wire.get("hiddenSteps").is_some());
}
#[test]
fn nested_evaluation_precedes_outer_and_callbacks_do_not_run() {
    let (_, view) = fixture(
        r#"function run() {
      receiver().write(inner(first()), second());
      register(() => forbidden());
      function nested() { alsoForbidden(); }
      class Hidden { method() { never(); } }
    }"#,
        "run",
    );
    let names = calls(&view.steps);
    assert_eq!(
        &names[..5],
        &["receiver", "first", "inner", "second", "receiver().write"]
    );
    assert!(names.contains(&"register"));
    assert!(
        !names
            .iter()
            .any(|s| ["forbidden", "alsoForbidden", "never"].contains(s))
    );
    assert!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.kind == "boundary")
            .count()
            >= 3
    );
}
#[test]
fn short_circuit_and_ternary_calls_stay_in_guarded_arms() {
    let (_, view) = fixture(
        "function run() { test() && yes(); other() || fallback(); value() ?? missing(); ok() ? left() : right(); }",
        "run",
    );
    assert_eq!(
        view.steps
            .iter()
            .filter(|s| s.kind == "call")
            .map(|s| s.label.as_str())
            .collect::<Vec<_>>(),
        vec!["test", "other", "value", "ok"]
    );
    let guards: Vec<_> = view.steps.iter().filter(|s| s.kind == "branch").collect();
    assert_eq!(guards.len(), 4);
    assert_eq!(calls(&guards[0].children), vec!["yes"]);
    assert_eq!(calls(&guards[3].children), vec!["left"]);
    assert_eq!(calls(&guards[3].alternate), vec!["right"]);
}
#[test]
fn exits_omit_unreachable_and_guard_remaining_paths() {
    let (_, view) = fixture(
        "function run(x) { if (x) return early(); later(); return done(); forbidden(); }",
        "run",
    );
    assert_eq!(view.steps.len(), 2);
    assert_eq!(view.steps[1].kind, "branch");
    assert!(view.steps[1].label.contains("continues normally"));
    assert_eq!(calls(&view.steps[1].children), vec!["later", "done"]);
    assert!(!calls(&view.steps).contains(&"forbidden"));
    assert!(view.warnings.iter().any(|s| s.contains("unreachable")));
    let (_, both) = fixture(
        "function run(x) { if(x) return a(); else throw b(); never(); }",
        "run",
    );
    assert!(!calls(&both.steps).contains(&"never"));
}
#[test]
fn loops_group_evaluation_without_unrolling() {
    let (_, view) = fixture(
        "function run() { for (let i = init(); test(i); advance()) { work(); } do { tick(); } while (again()); while (ready()) { consume(); } }",
        "run",
    );
    assert!(view.steps[0].label == "init" || view.steps[0].kind == "boundary");
    let loops: Vec<_> = view.steps.iter().filter(|s| s.kind == "loop").collect();
    assert_eq!(loops.len(), 3);
    assert_eq!(calls(&loops[0].children), vec!["test", "work", "advance"]);
    assert_eq!(calls(&loops[1].children), vec!["tick", "again"]);
}
#[test]
fn effects_and_unresolved_argument_effects_are_never_hidden() {
    let (g, view) = fixture(
        "function run() { this.state = value(); this.count++; this.count += 2; delete this.old; console.log(writeDb()); }",
        "run",
    );
    assert_eq!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.kind == "effect")
            .count(),
        4
    );
    assert!(calls(&view.steps).contains(&"writeDb"));
    assert_eq!(view.hidden_steps, 1);
    assert!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.hidden)
            .all(|s| s.label == "console.log")
    );
    let all = build_sequence(test_pin(7), &view.seed, &g.files[0], &g.calls, true).unwrap();
    assert_eq!(all.hidden_steps, 0);
    assert!(flatten(&all.steps).iter().all(|s| !s.hidden));
    assert_eq!(calls(&view.steps), calls(&all.steps));
}
#[test]
fn unsupported_and_optional_chain_fail_closed() {
    let (_, view) = fixture(
        "function run(x) { switch(x) { case 1: maybe(); break; } obj?.call(skipped()); }",
        "run",
    );
    assert!(calls(&view.steps).is_empty());
    assert!(
        view.warnings
            .iter()
            .any(|s| s.contains("Unsupported complex control"))
    );
    assert!(
        flatten(&view.steps)
            .iter()
            .any(|s| s.label.contains("Optional-chain"))
    );
}
#[test]
fn malformed_source_and_unsupported_language_fail_soft() {
    let (g, view) = fixture("function run() { good(); }", "run");
    let mut f = g.files[0].clone();
    f.text = "function run() { broken( }".into();
    let mut seed = view.seed.clone();
    seed.range.end_byte = f.text.len();
    let bad = build_sequence(test_pin(1), &seed, &f, &[], false).unwrap();
    assert!(calls(&bad.steps).is_empty());
    assert!(bad.steps.iter().any(|s| s.kind == "boundary"));
    f.language = "unsupported-language".into();
    assert!(build_sequence(test_pin(1), &seed, &f, &[], false).is_err());
}
#[test]
fn bounded_output_and_honest_visual_grouping() {
    let (_, view) = fixture(
        &format!("function run() {{ {} }}", "obj.work();".repeat(250)),
        "run",
    );
    assert!(view.truncated);
    assert!(flatten(&view.steps).len() <= 200);
    assert!(view.participants.len() <= 20);
    assert!(view.steps.iter().all(|step| step.target.is_none()));
    assert_eq!(view.participants[1].kind, "receiver");
    assert!(
        view.participants[1]
            .identification
            .contains("object identity")
    );
    let nested = format!(
        "function run() {{ {} work(); {} }}",
        "if(x){".repeat(40),
        "}".repeat(40)
    );
    let (_, deep) = fixture(&nested, "run");
    assert!(deep.truncated);
}

#[test]
fn computed_receiver_order_and_finally_completion() {
    let (_, view) = fixture("function run() { getObj()[key()](arg(inner())); }", "run");
    assert_eq!(
        calls(&view.steps),
        vec!["getObj", "key", "inner", "arg", "getObj()[key()]"]
    );
    let (_, view) = fixture(
        "function run() { try { return work(); } finally { cleanup(); } after(); }",
        "run",
    );
    assert_eq!(calls(&view.steps), vec!["work", "cleanup"]);
    let (_, view) = fixture(
        "function run() { try { throw err(); } catch(e) { recover(); } after(); }",
        "run",
    );
    assert_eq!(calls(&view.steps), vec!["err", "recover", "after"]);
    assert!(
        view.steps
            .last()
            .unwrap()
            .label
            .contains("continues normally")
    );
    let (_, view) = fixture(
        "function run() { try { work(); } finally { return stop(); } after(); }",
        "run",
    );
    assert_eq!(calls(&view.steps), vec!["work", "stop"]);
}
#[test]
fn optional_computed_access_does_not_execute_key_unconditionally() {
    let (_, view) = fixture(
        "function run() { const x = obj?.[key()]; value &&= compute(); }",
        "run",
    );
    assert!(!calls(&view.steps).contains(&"key"));
    let branch = view.steps.iter().find(|s| s.kind == "branch").unwrap();
    assert_eq!(calls(&branch.children), vec!["compute"]);
    assert!(branch.children.iter().any(|s| s.kind == "effect"));
}

#[test]
fn logging_in_guards_and_warning_calls_are_retained() {
    let (_, view) = fixture(
        "function run() { if(console.log(check())) work(); console.warn(warn()); console.error(error()); console.debug(debugArg()); }",
        "run",
    );
    let hidden: Vec<_> = flatten(&view.steps)
        .into_iter()
        .filter(|s| s.hidden)
        .map(|s| s.label.as_str())
        .collect();
    assert_eq!(hidden, vec!["console.debug"]);
    assert_eq!(view.hidden_steps, 1);
    assert!(
        flatten(&view.steps)
            .iter()
            .any(|s| s.label == "debugArg" && !s.hidden)
    );
}

#[test]
fn optional_detection_uses_syntax_not_argument_text() {
    let (_, view) = fixture(r#"function run() { fn("?."); outer(obj?.m()); }"#, "run");
    assert_eq!(calls(&view.steps), vec!["fn", "outer"]);
    assert!(
        flatten(&view.steps)
            .iter()
            .any(|s| s.label.contains("Optional-chain call") && s.call_id.is_some())
    );
}
#[test]
fn unsupported_loop_transfers_are_explicit_and_do_return_terminates() {
    for source in [
        "function run() { for(;test();advance()){continue;} }",
        "function run() { do {continue;} while(again()); }",
    ] {
        let (_, view) = fixture(source, "run");
        assert!(calls(&view.steps).is_empty());
        assert!(
            view.steps
                .iter()
                .any(|s| s.kind == "boundary" && s.label.contains("break/continue"))
        );
    }
    let (_, view) = fixture(
        "function run() { do { return done(); } while(test()); after(); }",
        "run",
    );
    assert_eq!(calls(&view.steps), vec!["done"]);
}
#[test]
fn for_of_computed_binding_and_destructuring_boundaries() {
    let (_, view) = fixture(
        "function run() { for(obj[key()] of values()) {work();} }",
        "run",
    );
    assert_eq!(calls(&view.steps), vec!["values", "key", "work"]);
    assert_eq!(view.steps[0].label, "values");
    assert_eq!(calls(&view.steps[1].children), vec!["key", "work"]);
    let (_, view) = fixture(
        "function run() { const {[key()]:x} = source(); ({[other()]:target} = rhs()); try{work();}catch({[catchKey()]: x = fallback()}) {recover();} }",
        "run",
    );
    assert_eq!(calls(&view.steps), vec!["source", "rhs", "work", "recover"]);
    let all = flatten(&view.steps);
    assert!(
        all.iter()
            .any(|s| s.label.contains("Destructuring binding"))
    );
    assert!(
        all.iter()
            .any(|s| s.label.contains("Destructuring assignment"))
    );
    assert!(all.iter().any(|s| s.label.contains("Catch destructuring")));
}

#[test]
fn participant_limit_keeps_source_backed_call_evidence() {
    let declarations = (0..25)
        .map(|i| format!("function f{i}() {{}} "))
        .collect::<String>();
    let invocations = (0..25).map(|i| format!("f{i}(); ")).collect::<String>();
    let (g, initial) = fixture(
        &format!("{declarations} function run() {{ {invocations} }}"),
        "run",
    );
    // Every measured call stays terminal, even if a declaration has the same name.
    let view = build_sequence(test_pin(7), &initial.seed, &g.files[0], &g.calls, false).unwrap();
    assert!(!view.truncated);
    assert!(view.participants.len() <= 20);
    let steps = flatten(&view.steps);
    assert_eq!(steps.iter().filter(|s| s.call_id.is_some()).count(), 25);
    for step in steps
        .iter()
        .filter(|s| s.kind == "boundary" && s.call_id.is_some())
    {
        assert!(step.target.is_none());
        assert!(
            g.calls
                .iter()
                .any(|c| Some(&c.id) == step.call_id.as_ref() && c.range == step.range)
        );
    }
}

fn participant_for<'a>(view: &'a SequenceView, call: &str) -> &'a trellis::behavior::Participant {
    let step = flatten(&view.steps)
        .into_iter()
        .find(|s| s.kind == "call" && s.label == call)
        .unwrap_or_else(|| panic!("missing {call}: {:?}", view.steps));
    assert!(step.target.is_none() && step.resolution.is_none());
    let receiver = call.split(['.', '[']).next().unwrap();
    view.participants
        .iter()
        .find(|p| p.label == receiver)
        .or_else(|| {
            view.participants.iter().find(|p| {
                p.kind == "import"
                    && (call == "read"
                        || call == "writeFile"
                        || call == "fs.stat"
                        || call == "C.open"
                        || call == "DefaultClient"
                        || call == "C")
            })
        })
        .or_else(|| view.participants.iter().find(|p| p.kind == "boundary"))
        .unwrap()
}
#[test]
fn standard_globals_are_identified_without_changing_measured_resolution() {
    let (graph, view) = fixture(
        "function run(x) { JSON.stringify(x); Math.abs(x); Object.keys(x); Array.from(x); String(x); Number(x); Boolean(x); BigInt(x); Symbol(x); Reflect.ownKeys(x); Promise.resolve(x); new Date(); new RegExp(x); new Error(x); }",
        "run",
    );
    let json = participant_for(&view, "JSON.stringify");
    assert_eq!((&*json.label, &*json.kind), ("JSON", "builtin"));
    for step in flatten(&view.steps)
        .into_iter()
        .filter(|s| s.kind == "call")
    {
        let measured = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == step.call_id.as_ref())
            .unwrap();
        assert!(step.resolution.is_none() && step.target.is_none());
        assert!(
            serde_json::to_value(measured)
                .unwrap()
                .get("target")
                .is_none()
        );
        assert_eq!(participant_for(&view, &step.label).kind, "builtin");
    }
}
#[test]
fn builtin_identification_respects_lexical_parameters_and_mutations() {
    for source in [
        "function run(JSON) { JSON.stringify(x); }",
        "function run() { JSON.stringify(x); const JSON = other; }",
        "const JSON = other; function run() { JSON.stringify(x); }",
        "function run({JSON}) { JSON.stringify(x); }",
        "function run({x: JSON}) { JSON.stringify(x); }",
        "function run([JSON]) { JSON.stringify(x); }",
        "function run() { const {JSON} = other; JSON.stringify(x); }",
        "function run() { JSON = other; JSON.stringify(x); }",
        "function run() { JSON.stringify = other; JSON.stringify(x); }",
        "function run() { JSON[key] = other; JSON.stringify(x); }",
        "function run() { ({JSON} = other); JSON.stringify(x); }",
        "function run() { ({x: JSON} = other); JSON.stringify(x); }",
        "function run() { globalThis.JSON = other; JSON.stringify(x); }",
        "function run() { delete JSON.stringify; JSON.stringify(x); }",
        "function run() { JSON++; JSON.stringify(x); }",
        "function run() { for(let JSON of things) { JSON.stringify(x); } }",
        "function run() { try {} catch(JSON) { JSON.stringify(x); } }",
        "function outer(JSON) { function run() { JSON.stringify(x); } }",
        r"const JSON = other; function run() { JSON.stringify(x); }",
        "function run() { { function JSON() {} } JSON.stringify(x); }",
        "function run() { (eval)('var JSON = other'); JSON.stringify(x); }",
        "function run() { (0, eval)('JSON = other'); JSON.stringify(x); }",
        "function run() { globalThis.eval('JSON = other'); JSON.stringify(x); }",
        "function run() { globalThis['eval']('JSON = other'); JSON.stringify(x); }",
        "function run() { const execute = eval; execute('JSON = other'); JSON.stringify(x); }",
    ] {
        let (_, view) = fixture(source, "run");
        assert_ne!(
            participant_for(&view, "JSON.stringify").kind,
            "builtin",
            "{source}"
        );
    }
    for source in [
        "function run() { function nested(JSON) {} JSON.stringify(x); }",
        "function run() { { const JSON = other; } JSON.stringify(x); }",
        "function run({JSON: other}) { JSON.stringify(x); }",
        "function unrelated(JSON) {} function run() { JSON.stringify(x); }",
    ] {
        let (_, view) = fixture(source, "run");
        assert_eq!(
            participant_for(&view, "JSON.stringify").kind,
            "builtin",
            "{source}"
        );
    }
}
#[test]
fn imports_group_by_module_and_never_infer_instance_types() {
    let (graph, view) = fixture(
        r#"import {readFile as read, writeFile} from 'node:fs/promises'; import * as fs from 'node:fs/promises'; import DefaultClient, {Client as C} from 'pkg'; function run() { read('x'); writeFile('x'); fs.stat('x'); C.open(); new DefaultClient(); const c = new C(); c.method(); missing(); }"#,
        "run",
    );
    let read = participant_for(&view, "read");
    assert_eq!((&*read.kind, &*read.label), ("import", "node:fs/promises"));
    assert_eq!(read.id, participant_for(&view, "writeFile").id);
    assert_eq!(read.id, participant_for(&view, "fs.stat").id);
    assert_eq!(participant_for(&view, "C.open").kind, "import");
    assert_eq!(participant_for(&view, "DefaultClient").kind, "import");
    // The variable initializer is now source-measured; its constructor call
    // remains a terminal step rather than an inferred internal target.
    let steps = flatten(&view.steps);
    let constructor = steps
        .iter()
        .find(|s| s.kind == "call" && s.label == "C")
        .unwrap();
    assert!(constructor.call_id.is_some());
    assert!(constructor.target.is_none() && constructor.resolution.is_none());
    assert_eq!(participant_for(&view, "C").kind, "import");
    let instance = participant_for(&view, "c.method");
    assert_eq!(instance.kind, "receiver");
    assert!(!instance.identification.contains("Client"));
    assert_eq!(participant_for(&view, "missing").kind, "boundary");
    let missing = steps
        .iter()
        .find(|s| s.kind == "call" && s.label == "missing")
        .unwrap();
    let measured = graph
        .calls
        .iter()
        .find(|call| missing.call_id.as_ref() == Some(&call.id))
        .unwrap();
    assert_eq!(measured.callee_text.as_deref(), Some("missing"));
    assert_eq!(measured.range, missing.range);
    let source = &graph
        .files
        .iter()
        .find(|file| file.path == missing.path)
        .unwrap()
        .text;
    assert_eq!(
        &source[missing.range.start_byte..missing.range.end_byte],
        "missing()"
    );
    let token = measured.callee_range.as_ref().unwrap();
    assert_eq!(&source[token.start_byte..token.end_byte], "missing");
    assert!(missing.target.is_none() && missing.resolution.is_none());
    for step in flatten(&view.steps)
        .into_iter()
        .filter(|s| s.call_id.is_some())
    {
        let measured = graph
            .calls
            .iter()
            .find(|c| Some(&c.id) == step.call_id.as_ref())
            .unwrap();
        assert!(step.resolution.is_none() && step.target.is_none());
        assert!(
            serde_json::to_value(measured)
                .unwrap()
                .get("target")
                .is_none()
        );
    }
}
#[test]
fn import_shadowing_mutation_and_computed_members_fail_closed() {
    for source in [
        "import {Client as C} from 'pkg'; function run(C) { C.open(); }",
        "import {Client as C} from 'pkg'; function run() { const {C} = other; C.open(); }",
        "import {Client as C} from 'pkg'; function run() { {let C; C.open();} }",
        "import {Client as C} from 'pkg'; function run() { C = other; C.open(); }",
        "import {Client as C} from 'pkg'; function run() { C.open = other; C.open(); }",
    ] {
        let (_, view) = fixture(source, "run");
        assert_eq!(
            participant_for(&view, "C.open").kind,
            "receiver",
            "{source}"
        );
    }
    let (_, view) = fixture(
        "import * as fs from 'fs'; function run(key) { JSON[key](); fs[key](); Buffer.from('x'); }",
        "run",
    );
    assert_eq!(participant_for(&view, "JSON[key]").kind, "boundary");
    assert_eq!(participant_for(&view, "fs[key]").kind, "boundary");
    assert_eq!(participant_for(&view, "Buffer.from").kind, "receiver");
    assert!(
        participant_for(&view, "Buffer.from")
            .identification
            .contains("Node.js")
    );
}
#[test]
fn external_participant_cap_preserves_every_measured_call() {
    let imports = (0..25)
        .map(|i| format!("import * as p{i} from 'pkg{i}';"))
        .collect::<String>();
    let body = (0..25).map(|i| format!("p{i}.call();")).collect::<String>();
    let (_, view) = fixture(&format!("{imports} function run() {{ {body} }}"), "run");
    assert!(view.truncated);
    assert_eq!(view.participants.len(), 20);
    assert_eq!(
        flatten(&view.steps)
            .iter()
            .filter(|s| s.call_id.is_some())
            .count(),
        25
    );
}
