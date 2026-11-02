//! Possible-path oracle for #542/#546/#558. No daemon or persisted index is involved.
use proptest::prelude::*;
use std::{
    collections::BTreeSet,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    behavior::{SequenceStep, build_sequence},
    indexer::{IndexOptions, index_workspace},
    model::{Graph, IndexPin},
};

const GUARD: &str = "only if the preceding path continues normally";
const TRY: &str = "?: continue only on success; otherwise early return";

#[derive(Clone, Debug)]
struct Case {
    operand: u8,
    branch: u8,
    prefix: bool,
    parens: bool,
}

fn strategy() -> impl Strategy<Value = Case> {
    (0u8..7, 0u8..6, any::<bool>(), any::<bool>()).prop_map(|(operand, branch, prefix, parens)| {
        Case {
            operand,
            branch,
            prefix,
            parens,
        }
    })
}

// Each name occurs once in the program. Construct the expected possible paths from the
// generated syntax, not from the measured calls or the sequence adapter.
fn program(c: &Case) -> (String, BTreeSet<Vec<String>>) {
    let mut states = vec![(Vec::<String>::new(), false)];
    let mut text = String::from("fn run(x: Option<u8>, flag: bool) -> Option<u8> { ");
    fn call(states: &mut [(Vec<String>, bool)], name: &str) {
        for (path, exited) in states {
            if !*exited {
                path.push(name.into());
            }
        }
    }
    fn may_exit(states: &mut Vec<(Vec<String>, bool)>) {
        let exits: Vec<_> = states
            .iter()
            .filter(|(_, e)| !e)
            .map(|(p, _)| (p.clone(), true))
            .collect();
        states.extend(exits);
    }
    if c.prefix {
        text.push_str("recv().method(arg(inner()), last()); ");
        for name in ["recv", "inner", "arg", "last", "method"] {
            call(&mut states, name);
        }
    }
    match c.operand {
        0 => {
            text.push_str(if c.parens {
                "(first())?; "
            } else {
                "first()?; "
            });
            call(&mut states, "first");
            may_exit(&mut states);
        }
        1 => {
            text.push_str("x?; ");
            may_exit(&mut states);
        }
        2 => {
            text.push_str("make().field?; ");
            call(&mut states, "make");
            may_exit(&mut states);
        }
        3 => {
            text.push_str("outer(x?, inside())?; ");
            may_exit(&mut states);
            call(&mut states, "inside");
            call(&mut states, "outer");
            may_exit(&mut states);
        }
        4 => {
            text.push_str("outer(first()?, inside())?; ");
            call(&mut states, "first");
            may_exit(&mut states);
            call(&mut states, "inside");
            call(&mut states, "outer");
            may_exit(&mut states);
        }
        5 => {
            text.push_str("receiver()?.method(argument())?; ");
            call(&mut states, "receiver");
            may_exit(&mut states);
            call(&mut states, "argument");
            call(&mut states, "method");
            may_exit(&mut states);
        }
        _ => {
            text.push_str("(side(), x).1?; ");
            call(&mut states, "side");
            may_exit(&mut states);
        }
    }
    match c.branch {
        0 => {
            text.push_str("if flag { return fail(); } else { other(); } ");
            let mut alt = states.clone();
            call(&mut states, "fail");
            for (_, e) in &mut states {
                *e = true;
            }
            call(&mut alt, "other");
            states.extend(alt);
        }
        1 => {
            text.push_str("match flag { true => return fail(), false => other() }; ");
            let mut alt = states.clone();
            call(&mut states, "fail");
            for (_, e) in &mut states {
                *e = true;
            }
            call(&mut alt, "other");
            states.extend(alt);
        }
        2 => {
            text.push_str("if flag { left(); } else { right(); } ");
            let mut alt = states.clone();
            call(&mut states, "left");
            call(&mut alt, "right");
            states.extend(alt);
        }
        3 => text.push_str("plain(); "),
        4 => {
            text.push_str("match flag { true => left(), false => right() }; ");
            let mut alt = states.clone();
            call(&mut states, "left");
            call(&mut alt, "right");
            states.extend(alt);
        }
        _ => {
            text.push_str("if flag { gated()?; } else { other(); } ");
            let mut alt = states.clone();
            call(&mut states, "gated");
            may_exit(&mut states);
            call(&mut alt, "other");
            states.extend(alt);
        }
    }
    if c.branch == 3 {
        call(&mut states, "plain");
    }
    text.push_str("after(); None }");
    call(&mut states, "after");
    (text, states.into_iter().map(|(p, _)| p).collect())
}

fn indexed(ext: &str, source: &str) -> (Graph, Vec<SequenceStep>) {
    let dir = tempfile::Builder::new()
        .prefix("sequence-folding-")
        .tempdir_in(env!("CARGO_MANIFEST_DIR"))
        .unwrap();
    std::fs::write(dir.path().join(format!("fixture.{ext}")), source).unwrap();
    let graph = index_workspace(
        &IndexOptions::new(dir.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let seed = graph.nodes.iter().find(|n| n.name == "run").unwrap();
    let pin = IndexPin {
        index_generation: uuid::Uuid::from_u128(0x00000000000040008000000000000001),
        index_revision: 7,
    };
    let view = build_sequence(pin, seed, &graph.files[0], &graph.calls, true).unwrap();
    assert!(!view.truncated, "{source}: {:?}", view.warnings);
    (graph, view.steps)
}

// A returned path may pass a continuation guard but cannot execute its children or a
// following sibling. A folded guard protects the rest of its own sibling list. Exits
// reaching the enclosing list must be guarded again there.
#[derive(Clone, Debug)]
struct Path {
    calls: Vec<String>,
    exited: bool,
    guarded: bool,
}
fn execute(steps: &[SequenceStep], mut paths: Vec<Path>) -> Result<Vec<Path>, String> {
    for step in steps {
        if step.label == GUARD {
            for p in &mut paths {
                if p.exited {
                    p.guarded = true;
                }
            }
            let mut done: Vec<_> = paths.iter().filter(|p| p.exited).cloned().collect();
            let live: Vec<_> = paths.into_iter().filter(|p| !p.exited).collect();
            done.extend(execute(&step.children, live)?);
            paths = done;
            continue;
        }
        if paths.iter().any(|p| p.exited && !p.guarded) {
            return Err(format!("unguarded suffix at {} ({})", step.label, step.id));
        }
        let mut done: Vec<_> = paths.iter().filter(|p| p.exited).cloned().collect();
        let mut live: Vec<_> = paths.into_iter().filter(|p| !p.exited).collect();
        if step.call_id.is_some() {
            for p in &mut live {
                p.calls.push(step.label.clone());
            }
        } else if step.kind == "return" {
            for p in &mut live {
                p.exited = true;
            }
        } else if step.label == TRY {
            let mut exits = live.clone();
            for p in &mut exits {
                p.exited = true;
            }
            live.extend(exits);
        } else if step.kind == "branch" && step.label.starts_with("if ") {
            let other = execute(&step.alternate, live.clone())?;
            live = execute(&step.children, live)?;
            live.extend(other);
        } else if step.kind == "branch" && step.label.starts_with("match:") {
            let mut arms = vec![];
            for arm in &step.alternate {
                arms.extend(execute(&arm.children, live.clone())?);
            }
            live = arms;
        } else if !step.children.is_empty() || !step.alternate.is_empty() {
            return Err(format!("unexpected control step: {step:#?}"));
        }
        for folded in &step.folded {
            if folded.label == TRY {
                let mut exits: Vec<_> = live.iter().filter(|p| !p.exited).cloned().collect();
                for p in &mut exits {
                    p.exited = true;
                }
                live.extend(exits);
            } else if folded.label == GUARD {
                for p in &mut live {
                    if p.exited {
                        p.guarded = true;
                    }
                }
            } else {
                return Err(format!("unexpected folded step: {folded:#?}"));
            }
        }
        done.extend(live);
        paths = done;
    }
    for p in &mut paths {
        p.guarded = false;
    }
    Ok(paths)
}

fn check(c: &Case) -> Result<(), String> {
    let (source, expected) = program(c);
    let (graph, steps) = indexed("rs", &source);
    fn visit<'a>(steps: &'a [SequenceStep], out: &mut Vec<&'a SequenceStep>) {
        for s in steps {
            out.push(s);
            visit(&s.children, out);
            visit(&s.alternate, out);
            visit(&s.folded, out);
        }
    }
    let mut all = vec![];
    visit(&steps, &mut all);
    let mut ids = BTreeSet::new();
    for s in all.iter().filter(|s| s.call_id.is_some()) {
        let id = s.call_id.as_ref().unwrap();
        if !ids.insert(id) {
            return Err(format!("duplicate call {id}: {source}"));
        }
        let measured = graph
            .calls
            .iter()
            .find(|m| &m.id == id)
            .ok_or("missing measured call")?;
        if s.range != measured.range || s.path != measured.path {
            return Err(format!("source identity changed: {s:?}"));
        }
    }
    if matches!(c.operand, 0 | 3..=5)
        && !all.iter().any(|s| {
            s.call_id.is_some()
                && s.folded
                    .iter()
                    .any(|f| f.label == TRY && f.alternate.iter().any(|s| s.kind == "return"))
        })
    {
        return Err(format!("no folded try with return evidence: {source}"));
    }
    let tried: &[&str] = match c.operand {
        0 => &["first"],
        3 => &["outer"],
        4 => &["first", "outer"],
        5 => &["receiver", "method"],
        _ => &[],
    };
    for name in tried {
        if !all.iter().any(|s| {
            s.call_id.is_some()
                && s.label == *name
                && s.folded
                    .iter()
                    .any(|f| f.label == TRY && f.alternate.iter().any(|r| r.kind == "return"))
        }) {
            return Err(format!(
                "direct operand {name} did not carry its folded try: {source}"
            ));
        }
    }
    if matches!(c.operand, 0 | 4 | 5)
        && !all
            .iter()
            .any(|s| s.call_id.is_some() && s.folded.iter().any(|f| f.label == GUARD))
    {
        return Err(format!(
            "no continuation guard folded into a call: {source}"
        ));
    }
    if matches!(c.operand, 1 | 2 | 6)
        && (!all.iter().any(|s| s.label == TRY)
            || all.iter().any(|s| {
                s.call_id.is_some()
                    && matches!(s.label.as_str(), "make" | "side")
                    && s.folded.iter().any(|f| f.label == TRY)
            }))
    {
        return Err(format!("value try folded: {source}"));
    }
    if (c.branch < 2 || c.branch == 5)
        && !all.iter().any(|s| {
            s.kind == "branch"
                && (s.label.starts_with("if ") || s.label.starts_with("match:"))
                && s.folded.iter().any(|f| f.label == GUARD)
        })
    {
        return Err(format!("no folded if/match guard: {source}"));
    }
    if matches!(c.branch, 2 | 4)
        && all.iter().any(|s| {
            s.kind == "branch"
                && (s.label.starts_with("if ") || s.label.starts_with("match:"))
                && !s.folded.is_empty()
        })
    {
        return Err(format!("non-exiting if was folded: {source}"));
    }
    let actual = execute(
        &steps,
        vec![Path {
            calls: vec![],
            exited: false,
            guarded: false,
        }],
    )
    .map_err(|e| format!("{e}\nsource: {source}\nsteps: {steps:#?}"))?
    .into_iter()
    .map(|p| p.calls)
    .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(format!(
            "source: {source}\nexpected: {expected:#?}\nactual: {actual:#?}\nsteps: {steps:#?}"
        ));
    }
    Ok(())
}

#[test]
fn fixed_witnesses() {
    for operand in 0..7 {
        for branch in 0..6 {
            for prefix in [false, true] {
                let c = Case {
                    operand,
                    branch,
                    prefix,
                    parens: true,
                };
                check(&c).unwrap_or_else(|e| panic!("{c:?}: {e}"));
            }
        }
    }
}

#[test]
fn other_languages_preserve_alternatives_and_folded_guards() {
    for (ext, source) in [
        (
            "js",
            "function run(flag) { receiver().method(inner(), last()); if (flag) { return fail(); } else { other(); } after(); }",
        ),
        (
            "py",
            "def run(flag):\n    receiver().method(inner(), last())\n    if flag:\n        return fail()\n    else:\n        other()\n    after()\n",
        ),
        (
            "java",
            "class Fixture { void run(boolean flag) { receiver().method(inner(), last()); if (flag) { return fail(); } else { other(); } after(); } }",
        ),
    ] {
        let (graph, steps) = indexed(ext, source);
        let branch = steps
            .iter()
            .find(|s| s.label.starts_with("if "))
            .unwrap_or_else(|| panic!("{ext}: {steps:#?}"));
        assert!(
            branch.folded.iter().any(|s| s.label == GUARD),
            "{ext}: {steps:#?}"
        );
        let actual: BTreeSet<_> = execute(
            &steps,
            vec![Path {
                calls: vec![],
                exited: false,
                guarded: false,
            }],
        )
        .unwrap_or_else(|e| panic!("{ext}: {e}\n{steps:#?}"))
        .into_iter()
        .map(|p| p.calls)
        .collect();
        let method = if ext == "js" {
            "receiver().method"
        } else {
            "method"
        };
        let expected: BTreeSet<Vec<String>> = [
            vec!["receiver", "inner", "last", method, "fail"],
            vec!["receiver", "inner", "last", method, "other", "after"],
        ]
        .into_iter()
        .map(|p| p.into_iter().map(String::from).collect())
        .collect();
        assert_eq!(actual, expected, "{ext}: {steps:#?}");
        fn evidence(steps: &[SequenceStep], graph: &Graph, seen: &mut BTreeSet<String>) {
            for s in steps {
                if let Some(id) = &s.call_id {
                    assert!(seen.insert(id.clone()), "duplicate call {id}");
                    let c = graph.calls.iter().find(|c| &c.id == id).unwrap();
                    assert_eq!((&s.path, &s.range), (&c.path, &c.range));
                }
                evidence(&s.children, graph, seen);
                evidence(&s.alternate, graph, seen);
                evidence(&s.folded, graph, seen);
            }
        }
        evidence(&steps, &graph, &mut BTreeSet::new());
    }
}

#[test]
fn opaque_exit_and_await_keep_their_guards() {
    for source in [
        "fn run(flag: bool) { if flag { custom!(); } after(); }",
        "async fn run() -> Option<u8> { request().await?; after(); None }",
    ] {
        let (_, steps) = indexed("rs", source);
        fn walk<'a>(steps: &'a [SequenceStep], found: &mut Vec<&'a SequenceStep>) {
            for s in steps {
                found.push(s);
                walk(&s.children, found);
                walk(&s.alternate, found);
                walk(&s.folded, found);
            }
        }
        let mut all = vec![];
        walk(&steps, &mut all);
        assert!(
            all.iter()
                .any(|s| s.label == GUARD && !s.children.is_empty()),
            "{source}: {steps:#?}"
        );
        assert!(
            all.iter().all(|s| s.folded.is_empty()),
            "{source}: {steps:#?}"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(32), failure_persistence: Some(Box::new(proptest::test_runner::FileFailurePersistence::Direct("tests/proptest-regressions/sequence_folding.txt"))), .. ProptestConfig::default() })]
    #[test]
    fn generated_paths(c in strategy()) { if let Err(e) = check(&c) { prop_assert!(false, "{:?}: {}", c, e); } }
}
