//! Generated syntax-only Sequence bodies. The small expression model is independent
//! of the language adapters: evaluate receiver, then arguments, then the invocation.
use proptest::prelude::*;
use proptest::test_runner::{Config, FileFailurePersistence, TestRunner};
use std::cell::Cell;
use std::{
    collections::HashSet,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    behavior::{SequenceStep, build_sequence},
    indexer::{IndexOptions, index_workspace},
    model::IndexPin,
};

#[derive(Clone, Debug)]
struct Expr {
    name: u8,
    receiver: Option<Box<Expr>>,
    args: Vec<Expr>,
}

fn expression() -> impl Strategy<Value = Expr> {
    (0u8..3)
        .prop_map(|name| Expr {
            name,
            receiver: None,
            args: vec![],
        })
        .prop_recursive(3, 24, 3, |inner| {
            (
                0u8..3,
                proptest::option::of(inner.clone()),
                proptest::collection::vec(inner, 0..3),
            )
                .prop_map(|(name, receiver, args)| Expr {
                    name,
                    receiver: receiver.map(Box::new),
                    args,
                })
        })
}

#[derive(Debug, PartialEq, Eq)]
struct Expected {
    start: usize,
    end: usize,
}

// Render from the generated tree and independently accumulate expected byte ranges
// in the evaluation order of this deliberately straight-line expression grammar.
fn render(expr: &Expr, text: &mut String, order: &mut Vec<Expected>) {
    let start = text.len();
    if let Some(receiver) = &expr.receiver {
        render(receiver, text, order);
        text.push('.');
    }
    text.push_str(["alpha", "beta", "gamma"][expr.name as usize]);
    text.push('(');
    for (i, arg) in expr.args.iter().enumerate() {
        if i != 0 {
            text.push_str(", ");
        }
        render(arg, text, order);
    }
    text.push(')');
    order.push(Expected {
        start,
        end: text.len(),
    });
}

fn measured<'a>(steps: &'a [SequenceStep], out: &mut Vec<&'a SequenceStep>) {
    for step in steps {
        if step.call_id.is_some() {
            out.push(step);
        }
        measured(&step.children, out);
        measured(&step.alternate, out);
    }
}

fn check(lang: &str, expressions: &[Expr]) {
    let (filename, prefix, separator, suffix) = match lang {
        "javascript" => ("fixture.js", "function run() {\n", ";\n", "}\n"),
        "rust" => ("fixture.rs", "fn run() {\n", ";\n", "}\n"),
        "java" => (
            "Fixture.java",
            "class Fixture { void run() {\n",
            ";\n",
            "} }\n",
        ),
        "python" => ("fixture.py", "def run():\n", "\n", ""),
        _ => unreachable!(),
    };
    let mut source = prefix.to_string();
    let mut expected = Vec::new();
    for expr in expressions {
        source.push_str("    ");
        render(expr, &mut source, &mut expected);
        source.push_str(separator);
    }
    source.push_str(suffix);
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(filename), &source).unwrap();
    let graph = index_workspace(
        &IndexOptions::new(dir.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        graph.stats.parse_error_files, 0,
        "{lang}: {source}\n{:?}",
        graph.diagnostics
    );
    let seed = graph.nodes.iter().find(|node| node.name == "run").unwrap();
    let pin = IndexPin {
        index_generation: uuid::Uuid::from_u128(0x00000000000040008000000000000001),
        index_revision: 1,
    };
    let view = build_sequence(pin, seed, &graph.files[0], &graph.calls, true).unwrap();
    assert!(!view.truncated, "{lang}: {source}");
    let mut actual = Vec::new();
    measured(&view.steps, &mut actual);
    let ranges: Vec<_> = actual
        .iter()
        .map(|step| Expected {
            start: step.range.start_byte,
            end: step.range.end_byte,
        })
        .collect();
    assert_eq!(
        ranges, expected,
        "{lang}: {source}\nsteps: {:#?}",
        view.steps
    );
    let mut seen = HashSet::new();
    for step in actual {
        let id = step.call_id.as_ref().unwrap();
        assert!(seen.insert(id), "duplicate call id {id}: {lang} {source}");
        let original = graph.calls.iter().find(|call| &call.id == id).unwrap();
        assert_eq!(step.range, original.range, "{lang}: {source}");
        assert_eq!(step.path, original.path);
        assert!(step.target.is_none() && step.resolution.is_none());
    }
}

fn languages() -> Vec<&'static str> {
    let only = std::env::var("TRELLIS_SEQUENCE_LANGUAGE").ok();
    let selected: Vec<_> = ["javascript", "rust", "java", "python"]
        .into_iter()
        .filter(|language| only.as_deref().is_none_or(|only| only == *language))
        .collect();
    assert!(
        !selected.is_empty(),
        "unknown TRELLIS_SEQUENCE_LANGUAGE: {only:?}"
    );
    selected
}

fn count(expr: &Expr) -> (usize, usize, usize) {
    let mut totals = (1, usize::from(expr.receiver.is_some()), expr.args.len());
    for child in expr
        .receiver
        .iter()
        .map(Box::as_ref)
        .chain(expr.args.iter())
    {
        let (calls, receivers, args) = count(child);
        totals.0 += calls;
        totals.1 += receivers;
        totals.2 += args;
    }
    totals
}

#[test]
fn generated_four_language_expression_orders() {
    let cases = std::env::var("TRELLIS_SEQUENCE_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    let mut runner = TestRunner::new(Config {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "proptest-regressions/sequence_building_properties.txt",
        ))),
        ..Config::default()
    });
    let totals = Cell::new((0usize, 0usize, 0usize, 0usize));
    runner
        .run(
            &proptest::collection::vec(expression(), 1..5),
            |expressions| {
                for language in languages() {
                    check(language, &expressions);
                }
                let (mut calls, mut receivers, mut args) = (0, 0, 0);
                for expr in &expressions {
                    let (c, r, a) = count(expr);
                    calls += c;
                    receivers += r;
                    args += a;
                }
                let (cases, old_calls, old_receivers, old_args) = totals.get();
                totals.set((
                    cases + 1,
                    old_calls + calls,
                    old_receivers + receivers,
                    old_args + args,
                ));
                Ok(())
            },
        )
        .unwrap();
    eprintln!(
        "generated successful cases, calls, receiver edges, argument edges (per language): {:?}",
        totals.get()
    );
}

#[test]
fn receiver_and_two_arguments_are_ordered_before_invocation() {
    let leaf = |name| Expr {
        name,
        receiver: None,
        args: vec![],
    };
    let root = Expr {
        name: 0,
        receiver: Some(Box::new(leaf(1))),
        args: vec![leaf(2), leaf(1)],
    };
    for language in languages() {
        check(language, std::slice::from_ref(&root));
    }
}

fn ranges(steps: &[SequenceStep]) -> Vec<Expected> {
    let mut calls = Vec::new();
    measured(steps, &mut calls);
    calls
        .iter()
        .map(|step| Expected {
            start: step.range.start_byte,
            end: step.range.end_byte,
        })
        .collect()
}

fn check_branch(lang: &str, parts: &[Expr; 4]) {
    let (filename, prefix, condition_end, then_end, else_end, suffix) = match lang {
        "javascript" => (
            "fixture.js",
            "function run() {\nif (",
            ") {\n",
            ";\n} else {\n",
            ";\n}\n",
            ";\n}\n",
        ),
        "java" => (
            "Fixture.java",
            "class Fixture { void run() {\nif (",
            ") {\n",
            ";\n} else {\n",
            ";\n}\n",
            ";\n} }\n",
        ),
        "rust" => (
            "fixture.rs",
            "fn run() {\nif ",
            " {\n",
            ";\n} else {\n",
            ";\n}\n",
            ";\n}\n",
        ),
        "python" => (
            "fixture.py",
            "def run():\n    if ",
            ":\n        ",
            "\n    else:\n        ",
            "\n    ",
            "\n",
        ),
        _ => unreachable!(),
    };
    let mut source = prefix.to_string();
    let mut expected = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    for (index, delimiter) in [condition_end, then_end, else_end, suffix]
        .into_iter()
        .enumerate()
    {
        render(&parts[index], &mut source, &mut expected[index]);
        source.push_str(delimiter);
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(filename), &source).unwrap();
    let graph = index_workspace(
        &IndexOptions::new(dir.path().to_owned()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(
        graph.stats.parse_error_files, 0,
        "{lang}: {source}\n{:?}",
        graph.diagnostics
    );
    let seed = graph.nodes.iter().find(|n| n.name == "run").unwrap();
    let pin = IndexPin {
        index_generation: uuid::Uuid::from_u128(0x00000000000040008000000000000001),
        index_revision: 1,
    };
    let view = build_sequence(pin, seed, &graph.files[0], &graph.calls, true).unwrap();
    assert!(!view.truncated, "{lang}: {source}");
    let at = view
        .steps
        .iter()
        .position(|s| s.kind == "branch" && s.label.starts_with("if "))
        .unwrap_or_else(|| panic!("missing if frame: {lang}: {source}\n{:#?}", view.steps));
    let branch = &view.steps[at];
    assert_eq!(
        ranges(&view.steps[..at]),
        expected[0],
        "condition {lang}: {source}"
    );
    assert_eq!(
        ranges(&branch.children),
        expected[1],
        "then {lang}: {source}"
    );
    assert_eq!(
        ranges(&branch.alternate),
        expected[2],
        "else {lang}: {source}"
    );
    assert_eq!(
        ranges(&view.steps[at + 1..]),
        expected[3],
        "suffix {lang}: {source}"
    );
}

#[test]
fn generated_branches_keep_calls_in_their_arms() {
    let cases = std::env::var("TRELLIS_SEQUENCE_CASES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    let mut runner = TestRunner::new(Config {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "proptest-regressions/sequence_branches.txt",
        ))),
        ..Config::default()
    });
    runner
        .run(
            &(expression(), expression(), expression(), expression()),
            |(a, b, c, d)| {
                for language in languages() {
                    check_branch(language, &[a.clone(), b.clone(), c.clone(), d.clone()]);
                }
                Ok(())
            },
        )
        .unwrap();
    eprintln!("generated branch cases per language: {cases}");
}

#[test]
fn branch_arms_remain_distinct() {
    let leaf = |name| Expr {
        name,
        receiver: None,
        args: vec![],
    };
    let parts = [leaf(0), leaf(1), leaf(2), leaf(0)];
    for language in languages() {
        check_branch(language, &parts);
    }
}
