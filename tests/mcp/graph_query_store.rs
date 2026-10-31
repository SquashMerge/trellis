#![cfg(feature = "test-support")]
//! Store-backed call-graph reads: one admitted snapshot, trusted published evidence.
use anyhow::Result;
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};
use trellis::{
    graph_query::{
        BoundaryReason, FrontierReason, GraphAnswer, GraphQuery, GraphResult, GraphSource, Visit,
        WarningCode,
    },
    semantic_evidence::{self as s, CoverageState, Dispatch},
    store::Store,
};

use crate::graph_fixtures;
use crate::semantic_batch_support;
use graph_fixtures::{Bind, Publisher, Spec, bind, fanout_source, syntax_id};
use semantic_batch_support::{BatchFixture, DocSpec};

const CHAIN: &str = "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\n";

fn query(root: &str, producer: bool) -> GraphQuery {
    GraphQuery {
        root_syntax_id: root.into(),
        semantic_producer_id: producer.then(|| graph_fixtures::PRODUCER.into()),
        depth: 2,
        max_nodes: 150,
        max_calls: 500,
        include_external: true,
        include_tests: true,
    }
}

fn run(store: &Store, query: &GraphQuery) -> Result<GraphAnswer> {
    store
        .evidence_response()?
        .graph_query(query, None, &mut || Ok(()))
}

fn result(answer: GraphAnswer) -> GraphResult {
    answer.outcome.expect("contract success")
}

fn index_db(state: &Path) -> PathBuf {
    std::fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db")
}

fn sql(state: &Path, statements: &str) {
    let db = rusqlite::Connection::open(index_db(state)).unwrap();
    db.execute_batch(statements).unwrap();
}

fn workspace() -> (tempfile::TempDir, tempfile::TempDir) {
    (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap())
}

fn warning_keys(result: &GraphResult) -> Vec<(WarningCode, Option<String>)> {
    result
        .warnings
        .iter()
        .map(|w| (w.code, w.provenance_id.clone()))
        .collect()
}

#[test]
fn graph_answer_is_single_revision_at_head() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", "fn a() { b(); }\nfn b() {}\nfn c() {}\n")]);
    spec.bindings = vec![bind("a", 0, "b")];
    let r1 = publisher.publish(&spec);
    let mut spec = Spec::new(&[("a.rs", "fn a() { c(); }\nfn b() {}\nfn c() {}\n")]);
    spec.bindings = vec![bind("a", 0, "c")];
    let r2 = publisher.publish(&spec);
    assert_ne!(r1, r2);
    let native = publisher.native();
    let revision = native.revision.id.clone();
    let answer = run(&publisher.store, &query(&syntax_id(native, "a"), true)).unwrap();
    assert_eq!(answer.basis, r2);
    assert_eq!(answer.requested.revision_id, revision);
    assert_eq!(
        answer.requested.source_set_id,
        native.revision.source_set_id
    );
    let result = result(answer);
    assert_eq!(result.resolved_revision_id, revision);
    let ids: Vec<_> = result
        .nodes
        .iter()
        .map(|n| n.declaration.syntax_id.clone())
        .collect();
    assert_eq!(ids, vec![syntax_id(native, "a"), syntax_id(native, "c")]);
    assert_eq!(result.edges.len(), 1);
    assert_eq!(result.edges[0].visit, Visit::New);
    assert_eq!(result.edges[0].to, Some(syntax_id(native, "c")));
    for node in &result.nodes {
        assert_eq!(node.declaration.revision_id, revision);
    }
    for edge in &result.edges {
        assert_eq!(edge.call.revision_id, revision);
        let binding = edge.binding.as_ref().unwrap();
        assert_eq!(binding.join.anchor.revision_id, revision);
    }
    assert!(!result.coverage.is_empty() && !result.provenance.is_empty());
    assert!(result.coverage.iter().all(|c| c.revision_id == revision));
    assert!(result.provenance.iter().all(|p| p.revision_id == revision));
    assert!(
        result
            .provenance
            .iter()
            .all(|p| p.freshness == s::Freshness::Fresh)
    );
}

#[test]
fn stale_expected_basis_conflicts() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let spec = Spec::new(&[("a.rs", CHAIN)]);
    let r1 = publisher.publish(&spec);
    let r2 = publisher.publish(&Spec::new(&[(
        "a.rs",
        "fn a() { b(); }\nfn b() {}\nfn c() {}\n",
    )]));
    let root_id = syntax_id(publisher.native(), "a");
    let response = publisher.store.evidence_response().unwrap();
    let error = response
        .graph_query(&query(&root_id, true), Some(r1), &mut || Ok(()))
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("revision conflict: expected basis differs from admitted head")
    );
    let answer = response
        .graph_query(&query(&root_id, true), Some(r2), &mut || Ok(()))
        .unwrap();
    assert_eq!(answer.basis, r2);
}

#[test]
fn direct_and_constructor_expand_virtual_does_not() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[(
        "a.rs",
        "fn a() { b(); c(); d(); }\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n",
    )]);
    let constructor = Bind {
        dispatch: Dispatch::Constructor,
        ..bind("a", 1, "c")
    };
    let virtual_call = Bind {
        dispatch: Dispatch::Virtual,
        possible: vec!["e".into()],
        ..bind("a", 2, "d")
    };
    spec.bindings = vec![bind("a", 0, "b"), constructor, virtual_call];
    publisher.publish(&spec);
    let native = publisher.native();
    let result = result(run(&publisher.store, &query(&syntax_id(native, "a"), true)).unwrap());
    let reasons: Vec<_> = result
        .edges
        .iter()
        .map(|e| (e.visit, e.boundary_reason))
        .collect();
    assert_eq!(
        reasons,
        vec![
            (Visit::New, BoundaryReason::None),
            (Visit::New, BoundaryReason::None),
            (Visit::Boundary, BoundaryReason::Dispatch),
        ]
    );
    let ids: Vec<_> = result
        .nodes
        .iter()
        .map(|n| n.declaration.syntax_id.clone())
        .collect();
    assert_eq!(
        ids,
        vec![
            syntax_id(native, "a"),
            syntax_id(native, "b"),
            syntax_id(native, "c")
        ]
    );
    assert_eq!(result.edges[2].to, None);
}

/// #483: the CLI query walks traversable bindings (`boundaryReason: none`) to its depth, as
/// the MCP call graph does. Depth 0 keeps only the seed's own calls; the node limit counts
/// omitted targets.
#[test]
fn query_view_walks_direct_bindings_to_its_depth() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", CHAIN)]);
    spec.bindings = vec![bind("a", 0, "b"), bind("b", 0, "c")];
    publisher.publish(&spec);
    let id = |name: &str| {
        publisher
            .graph
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|node| node.name == name)
            .unwrap()
            .id
            .clone()
    };
    let (a, b, c) = (id("a"), id("b"), id("c"));
    let view = |depth: usize, max_nodes: usize| {
        let query: trellis::model::ViewQuery = serde_json::from_value(serde_json::json!({
            "seed": a, "depth": depth, "maxNodes": max_nodes
        }))
        .unwrap();
        publisher.store.query_view(&query).unwrap().unwrap()
    };
    let nodes = |view: &trellis::model::ViewResult| {
        view.nodes.iter().map(|n| n.id.clone()).collect::<Vec<_>>()
    };
    let callers = |view: &trellis::model::ViewResult| {
        view.calls
            .iter()
            .map(|call| call.caller.clone())
            .collect::<Vec<_>>()
    };
    let seed_only = view(0, 40);
    assert_eq!(nodes(&seed_only), std::slice::from_ref(&a));
    assert_eq!(callers(&seed_only), std::slice::from_ref(&a));
    let one = view(1, 40);
    assert_eq!(nodes(&one), [a.clone(), b.clone()]);
    assert_eq!(callers(&one), std::slice::from_ref(&a));
    assert_eq!(one.semantic.bindings[0].target_id.as_ref(), Some(&b));
    let deep = view(5, 40);
    assert_eq!(nodes(&deep), [a.clone(), b.clone(), c.clone()]);
    assert_eq!(callers(&deep), [a.clone(), b.clone()]);
    assert_eq!(deep.semantic.bindings.len(), 2);
    assert!(!deep.truncated);
    let bounded = view(5, 1);
    assert_eq!(nodes(&bounded), std::slice::from_ref(&a));
    assert_eq!(bounded.omitted_nodes, 1);
    assert!(bounded.truncated);
}

fn sequence_steps(
    steps: &[trellis::behavior::SequenceStep],
) -> Vec<&trellis::behavior::SequenceStep> {
    let mut out = vec![];
    for step in steps {
        if step.call_id.is_some() {
            out.push(step);
        }
        out.extend(sequence_steps(&step.children));
        out.extend(sequence_steps(&step.alternate));
    }
    out
}

#[test]
fn sequence_enriches_every_occurrence_without_graph_grouping() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        (
            "a.rs",
            "fn a() { lexical_decoy(); lexical_decoy(); }\nfn lexical_decoy() {}\n",
        ),
        ("target.rs", "fn actual_target() {}\n"),
    ]);
    spec.bindings = (0..2)
        .map(|nth| Bind {
            provenance: "shared-calls".into(),
            ..bind("a", nth, "actual_target")
        })
        .collect();
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let target = syntax_id(publisher.native(), "actual_target");
    let graph = result(run(&publisher.store, &query(&seed, true)).unwrap());
    assert_eq!(graph.edges.len(), 1);
    assert_eq!(graph.edges[0].call_site_count, 2);
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    let steps = sequence_steps(&view.steps);
    assert_eq!(steps.len(), 2);
    assert_ne!(steps[0].call_id, steps[1].call_id);
    for (step, call) in steps
        .iter()
        .zip(graph_fixtures::calls_of(publisher.native(), "a"))
    {
        assert_eq!(step.call_id.as_deref(), Some(call.id.as_str()));
        assert_eq!(step.target.as_deref(), Some(target.as_str()));
        let evidence = step.call_evidence.as_ref().unwrap();
        assert_eq!(evidence.boundary_reason, BoundaryReason::None);
        assert_eq!(evidence.resolution, Some(s::Resolution::Resolved));
        assert_eq!(evidence.freshness, Some(s::Freshness::Fresh));
        assert_eq!(evidence.provenance_id.as_deref(), Some("shared-calls"));
        assert!(step.resolution.is_none());
    }
    let participant = view.participants.iter().find(|p| p.id == target).unwrap();
    assert_eq!(participant.declaration.as_ref().unwrap().path, "target.rs");
    assert_eq!(view.revision, pin);
    assert!(view.semantic_warnings.is_empty());
    let syntax = trellis::behavior::build_sequence(
        pin,
        &view.seed,
        &publisher
            .store
            .source_at("a.rs", Some(pin))
            .unwrap()
            .unwrap()
            .1,
        &publisher.graph.as_ref().unwrap().calls,
        false,
    )
    .unwrap();
    fn strip(steps: &mut [trellis::behavior::SequenceStep]) {
        for step in steps {
            step.target = None;
            step.call_evidence = None;
            strip(&mut step.children);
            strip(&mut step.alternate);
        }
    }
    let mut enriched = view.steps.clone();
    strip(&mut enriched);
    assert_eq!(enriched, syntax.steps);
    std::fs::write(
        root.path().join("target.rs"),
        "changed outside published snapshot",
    )
    .unwrap();
    assert_eq!(
        publisher
            .store
            .sequence_at(&seed, pin, false)
            .unwrap()
            .unwrap(),
        view
    );
}

#[test]
fn sequence_uses_engine_boundary_precedence_and_warnings() {
    for (mode, reason) in [
        ("ambiguous", BoundaryReason::Ambiguous),
        ("unresolved", BoundaryReason::Unresolved),
        ("external", BoundaryReason::External),
        ("dispatch", BoundaryReason::Dispatch),
    ] {
        let (state, root) = workspace();
        let mut publisher = Publisher::open(state.path(), root.path());
        let mut spec = Spec::new(&[("a.rs", "fn a() { b(); b(); }\nfn b() {}\n")]);
        spec.bindings = (0..2)
            .map(|nth| Bind {
                provenance: "shared".into(),
                ..bind("a", nth, "b")
            })
            .collect();
        match mode {
            "ambiguous" => {
                for b in &mut spec.bindings {
                    b.resolution = s::Resolution::Ambiguous;
                }
            }
            "unresolved" => {
                for b in &mut spec.bindings {
                    b.resolution = s::Resolution::Unresolved;
                }
            }
            "external" => {
                for b in &mut spec.bindings {
                    b.resolution = s::Resolution::External;
                }
            }
            "dispatch" => {
                for b in &mut spec.bindings {
                    b.dispatch = Dispatch::Virtual;
                    b.possible = vec!["b".into()];
                }
            }
            _ => unreachable!(),
        }
        let pin = publisher.publish(&spec);
        let seed = syntax_id(publisher.native(), "a");
        let view = publisher
            .store
            .sequence_at(&seed, pin, false)
            .unwrap()
            .unwrap();
        assert_eq!(
            view.semantic.tier,
            trellis::behavior::EvidenceTier::Semantic,
            "{mode}"
        );
        let steps = sequence_steps(&view.steps);
        assert_eq!(steps.len(), 2, "{mode}");
        // External calls are incidental (#522): kept with their evidence, but hidden.
        let external = mode == "external";
        assert_eq!(
            (view.hidden_steps, view.incidental_steps),
            if external { (2, 2) } else { (0, 0) },
            "{mode}"
        );
        assert!(steps.iter().all(|step| step.hidden == external), "{mode}");
        assert!(
            steps.iter().all(|step| step.target.is_none()
                && step.call_evidence.as_ref().unwrap().boundary_reason == reason),
            "{mode}"
        );
        let graph = result(run(&publisher.store, &query(&seed, true)).unwrap());
        assert_eq!(view.semantic_warnings, graph.warnings, "{mode}");
        assert!(
            graph
                .edges
                .iter()
                .all(|edge| edge.boundary_reason == reason)
        );
    }
}

/// #522: a seed whose calls have no fresh selected binding keeps the syntax view (its
/// source-hint lanes included), and says why, instead of drawing every call as a boundary.
#[test]
fn sequence_without_fresh_bindings_falls_back_to_syntax_with_its_reason() {
    for (mode, reason) in [
        (
            "stale",
            "Semantic evidence for a.rs is stale: it was imported for an earlier version of the file. Showing syntax.",
        ),
        (
            "missing",
            "No semantic binding covers this method's calls; showing syntax.",
        ),
        (
            "failed",
            "Semantic coverage of a.rs is failed; showing syntax.",
        ),
    ] {
        let (state, root) = workspace();
        let mut publisher = Publisher::open(state.path(), root.path());
        let source = "fn a() { client.send(); b(); }\nfn b() {}\n";
        let mut spec = Spec::new(&[("a.rs", source)]);
        spec.bindings = vec![bind("a", 0, "b"), bind("a", 1, "b")];
        match mode {
            "stale" => spec.requested_version = "v2".into(),
            "missing" => spec.bindings.clear(),
            "failed" => {
                spec.states.insert("a.rs".into(), CoverageState::Failed);
            }
            _ => unreachable!(),
        }
        let pin = publisher.publish(&spec);
        let seed = syntax_id(publisher.native(), "a");
        let view = publisher
            .store
            .sequence_at(&seed, pin, false)
            .unwrap()
            .unwrap();
        assert_eq!(view.semantic.tier, trellis::behavior::EvidenceTier::Syntax);
        assert_eq!(view.semantic.reason.as_deref(), Some(reason), "{mode}");
        let steps = sequence_steps(&view.steps);
        assert_eq!(steps.len(), 2, "{mode}");
        assert!(
            steps
                .iter()
                .all(|step| step.target.is_none() && step.call_evidence.is_none()),
            "{mode}"
        );
        // Exactly the syntax DTO: source-hint lanes and all.
        let mut syntax = trellis::behavior::build_sequence(
            pin,
            &view.seed,
            &publisher
                .store
                .source_at("a.rs", Some(pin))
                .unwrap()
                .unwrap()
                .1,
            &publisher.graph.as_ref().unwrap().calls,
            false,
        )
        .unwrap();
        syntax.semantic = view.semantic.clone();
        assert!(syntax.participants.len() > 1, "{mode}");
        assert_eq!(view, syntax, "{mode}");
    }
}

#[test]
fn sequence_retains_exact_selected_snapshot_and_self_target() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", "fn a() { b(); }\nfn b() {}\n")]);
    spec.bindings = vec![bind("a", 0, "a")];
    let r1 = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let old = publisher
        .store
        .sequence_at(&seed, r1, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        sequence_steps(&old.steps)[0].target.as_deref(),
        Some(seed.as_str())
    );
    assert_eq!(
        old.participants
            .iter()
            .filter(|p| p.declaration.is_some())
            .count(),
        1
    );
    spec.bindings = vec![bind("a", 0, "b")];
    let r2 = publisher.publish(&spec);
    let new = publisher
        .store
        .sequence_at(&seed, r2, false)
        .unwrap()
        .unwrap();
    assert_ne!(
        sequence_steps(&new.steps)[0].target,
        sequence_steps(&old.steps)[0].target
    );
    assert_eq!(
        publisher
            .store
            .sequence_at(&seed, r1, false)
            .unwrap()
            .unwrap(),
        old
    );
}

#[test]
fn sequence_zero_and_multiple_producers_are_syntax_only() {
    for multiple in [false, true] {
        let (state, root) = workspace();
        let (pin, seed) = {
            let mut publisher = Publisher::open(state.path(), root.path());
            let mut spec = if multiple {
                Spec::new(&[("a.rs", CHAIN)])
            } else {
                Spec::syntax_only(&[("a.rs", CHAIN)])
            };
            if multiple {
                spec.bindings = vec![bind("a", 0, "b")];
            }
            let pin = if multiple {
                publisher.publish_with_semantic(&spec, graph_fixtures::add_second_producer)
            } else {
                publisher.publish(&spec)
            };
            (pin, syntax_id(publisher.native(), "a"))
        };
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let view = store.sequence_at(&seed, pin, false).unwrap().unwrap();
        assert!(
            sequence_steps(&view.steps)
                .iter()
                .all(|step| step.target.is_none() && step.call_evidence.is_none())
        );
        assert_eq!(
            view.warnings
                .iter()
                .any(|warning| *warning == trellis::store::multiple_producers_note("rust")),
            multiple
        );
    }
}

#[test]
fn sequence_outside_sole_producer_languages_preserves_plain_syntax() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let files = [
        ("src/lib.rs", "fn lib() {}\n"),
        (
            "web/a.js",
            "function r(ok) { if (ok) client.send(); other.close(); }\n",
        ),
    ];
    let syntax_pin = publisher.publish(&Spec::syntax_only(&files));
    let seed = syntax_id(publisher.native(), "r");
    let syntax: Vec<_> = [false, true]
        .into_iter()
        .map(|show_all| {
            publisher
                .store
                .sequence_at(&seed, syntax_pin, show_all)
                .unwrap()
                .unwrap()
        })
        .collect();
    assert!(syntax.iter().all(|view| view.participants.len() > 1));
    // Spec::new publishes a Rust-only producer alongside the JavaScript seed.
    assert!(syntax.iter().all(
        |view| view.semantic.reason.as_deref() == Some(trellis::graph_query::NO_PRODUCER_NOTE)
    ));
    let semantic_pin = publisher.publish(&Spec::new(&files));
    for (show_all, mut expected) in [false, true].into_iter().zip(syntax) {
        expected.revision = semantic_pin;
        expected.semantic.reason = Some(trellis::graph_query::LANGUAGE_NOTE.into());
        let actual = publisher
            .store
            .sequence_at(&seed, semantic_pin, show_all)
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}

#[test]
fn sequence_same_named_targets_keep_exact_declarations_and_constructor_dispatch() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        ("a.rs", "fn a() { b(); b(); }\nfn b() {}\n"),
        ("other.rs", "fn b() {}\n"),
    ]);
    publisher.publish(&spec);
    let native = publisher.native();
    let targets: Vec<_> = native
        .declarations
        .iter()
        .filter(|d| d.name.as_deref() == Some("b") && d.name_range.is_some())
        .map(|d| s::Target::Internal {
            syntax_id: d.syntax_id.clone(),
            document: d.document.clone(),
            revision_id: native.revision.id.clone(),
        })
        .collect();
    assert_eq!(targets.len(), 2);
    spec.bindings = vec![
        bind("a", 0, "b"),
        Bind {
            dispatch: Dispatch::Constructor,
            ..bind("a", 1, "b")
        },
    ];
    let pin = publisher.publish_with_semantic(&spec, |publication, _| {
        for (binding, target) in publication.call_bindings.iter_mut().zip(targets) {
            binding.declared_target = Some(target);
        }
    });
    let seed = syntax_id(publisher.native(), "a");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    let steps = sequence_steps(&view.steps);
    assert_ne!(steps[0].target, steps[1].target);
    assert_eq!(
        steps[1].call_evidence.as_ref().unwrap().dispatch,
        Some(Dispatch::Constructor)
    );
    let declarations: Vec<_> = view
        .participants
        .iter()
        .filter_map(|p| p.declaration.as_ref())
        .filter(|d| d.name == "b")
        .collect();
    assert_eq!(declarations.len(), 2);
    assert_ne!(declarations[0].id, declarations[1].id);
    assert_ne!(declarations[0].path, declarations[1].path);
}

#[test]
fn sequence_constructor_can_target_exact_class_declaration() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.py", "class B:\n    pass\ndef a():\n    B()\n")]);
    spec.languages = vec![s::Language::Python];
    spec.bindings = vec![Bind {
        dispatch: Dispatch::Constructor,
        ..bind("a", 0, "B")
    }];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let target = syntax_id(publisher.native(), "B");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert_eq!(
        sequence_steps(&view.steps)[0].target.as_deref(),
        Some(target.as_str())
    );
    let participant = view.participants.iter().find(|p| p.id == target).unwrap();
    assert_eq!(participant.kind, "class");
    assert_eq!(
        participant.declaration.as_ref().unwrap().kind,
        trellis::model::SymbolKind::Class
    );
}

/// #522: a direct callee whose body would take the view past 200 steps keeps its lane and
/// arrow but is not expanded; the view says so and is truncated.
#[test]
fn sequence_expansion_respects_the_step_bound() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let body = "x(); ".repeat(200);
    let source = format!("fn a() {{ big(); }}\nfn big() {{ {body}}}\nfn x() {{}}\n");
    let mut spec = Spec::new(&[("a.rs", &source)]);
    spec.bindings = vec![bind("a", 0, "big")];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let big = syntax_id(publisher.native(), "big");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    let steps = sequence_steps(&view.steps);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].target.as_deref(), Some(big.as_str()));
    assert!(steps[0].children.is_empty());
    assert!(view.truncated);
    assert!(
        view.warnings
            .iter()
            .any(|w| w == "Sequence step limit reached; the body of big is not expanded.")
    );
}

/// #522 review: a repository-defined `console.log` the JS adapter hides by spelling, bound
/// by fresh exact direct evidence to its local method, is shown with its expanded body.
#[test]
fn sequence_resolved_local_call_is_not_hidden_by_a_spelling_heuristic() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let source = "const console = { log() { helper(); } };\nfunction helper() {}\nfunction visible() {}\nfunction a() { console.log(); visible(); }\n";
    let mut spec = Spec::new(&[("a.js", source)]);
    spec.languages = vec![s::Language::Javascript];
    spec.bindings = vec![bind("a", 0, "log"), bind("log", 0, "helper")];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let (log, helper) = (
        syntax_id(publisher.native(), "log"),
        syntax_id(publisher.native(), "helper"),
    );
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    let call = sequence_steps(&view.steps)
        .into_iter()
        .find(|step| step.target.as_deref() == Some(log.as_str()))
        .expect("console.log resolves to the local method");
    assert!(!call.hidden);
    assert_eq!(call.children.len(), 1, "{view:?}");
    assert_eq!(call.children[0].target.as_deref(), Some(helper.as_str()));
    assert!(!call.children[0].hidden);
    assert_eq!((view.hidden_steps, view.incidental_steps), (0, 0));
}

/// The `nth` call of `caller` bound to an external (standard-library) declaration.
fn bind_external(caller: &str, nth: usize) -> Bind {
    Bind {
        resolution: s::Resolution::External,
        dispatch: Dispatch::Unknown,
        target: None,
        ..bind(caller, nth, "")
    }
}

/// Every step below `steps`, and the shown ones (a hidden step hides what it nests).
fn counts(steps: &[trellis::behavior::SequenceStep]) -> (usize, usize) {
    steps.iter().fold((0, 0), |(all, shown), step| {
        let (children, children_shown) = counts(&step.children);
        let (alternate, alternate_shown) = counts(&step.alternate);
        (
            all + 1 + children + alternate,
            shown + usize::from(!step.hidden) * (1 + children_shown + alternate_shown),
        )
    })
}

const COORDINATE_CALLEES: [&str; 4] = ["receive", "start_final_check", "phase_hook", "changed"];

/// A `coordinate`-shaped method (#536): more syntax steps than the visible bound, almost all
/// of them standard-library calls, with its four local callees last.
fn coordinate_spec(incidental: usize) -> Spec {
    let std: String = (0..incidental).map(|n| format!("x{n}(); ")).collect();
    let callees: String = COORDINATE_CALLEES
        .map(|name| format!("{name}(); "))
        .concat();
    let bodies: String = COORDINATE_CALLEES
        .map(|name| format!("fn {name}() {{ y(); leaf(); }}\n"))
        .concat();
    let source = format!("fn coordinate() {{ {std}{callees}}}\n{bodies}fn leaf() {{}}\n");
    let mut spec = Spec::new(&[("a.rs", &source)]);
    spec.bindings = (0..incidental)
        .map(|nth| bind_external("coordinate", nth))
        .collect();
    for (nth, name) in COORDINATE_CALLEES.iter().enumerate() {
        spec.bindings
            .push(bind("coordinate", incidental + nth, name));
        spec.bindings.push(bind_external(name, 0));
        spec.bindings.push(bind(name, 1, "leaf"));
    }
    spec
}

/// #536: hidden incidental calls do not use the 200 visible steps, so a method with more
/// syntax steps than that still expands its callee lanes.
#[test]
fn sequence_hidden_incidental_steps_do_not_use_the_visible_bound() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let pin = publisher.publish(&coordinate_spec(240));
    let seed = syntax_id(publisher.native(), "coordinate");
    let leaf = syntax_id(publisher.native(), "leaf");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert!(!view.truncated, "{:?}", view.warnings);
    assert!(view.truncated_by.is_empty());
    assert_eq!(
        view.semantic.tier,
        trellis::behavior::EvidenceTier::Semantic
    );
    // 244 seed steps and 4 two-step callee bodies; only the lanes' calls are shown.
    assert_eq!(counts(&view.steps), (252, 8));
    assert_eq!((view.incidental_steps, view.hidden_steps), (244, 244));
    for name in COORDINATE_CALLEES {
        let callee = syntax_id(publisher.native(), name);
        let step = view
            .steps
            .iter()
            .find(|step| step.target.as_deref() == Some(callee.as_str()))
            .unwrap_or_else(|| panic!("{name} has a lane"));
        assert!(!step.hidden);
        assert_eq!(step.children.len(), 2, "{name} is expanded");
        assert!(
            step.children
                .iter()
                .all(|child| child.lane.as_deref() == Some(&*callee))
        );
        assert!(step.children[0].hidden, "its std call is incidental");
        assert_eq!(step.children[1].target.as_deref(), Some(leaf.as_str()));
    }
}

/// #536: Show all still respects the visible bound: the shown prefix stops at 200 steps and
/// the view is truncated with the typed `visibleSteps` reason.
#[test]
fn sequence_show_all_truncates_at_the_visible_bound() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let pin = publisher.publish(&coordinate_spec(240));
    let seed = syntax_id(publisher.native(), "coordinate");
    let view = publisher
        .store
        .sequence_at(&seed, pin, true)
        .unwrap()
        .unwrap();
    assert!(view.truncated);
    assert_eq!(
        view.truncated_by,
        [trellis::behavior::SequenceLimit::VisibleSteps]
    );
    assert_eq!(counts(&view.steps), (200, 200));
    assert_eq!((view.incidental_steps, view.hidden_steps), (200, 0));
    assert!(view.steps.iter().all(|step| step.target.is_none()));
    assert!(view.warnings.iter().any(|w| w.contains("200 shown steps")));
    let json = serde_json::to_value(&view).unwrap();
    assert_eq!(json["truncatedBy"], serde_json::json!(["visibleSteps"]));
}

/// #536: a callee body is hidden before it is admitted, so a body of 250 incidental calls
/// expands; with Show all those calls are shown, so the same body does not fit and is
/// refused with the typed `visibleSteps` reason.
#[test]
fn sequence_callee_incidental_steps_use_no_visible_room() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let std: String = (0..250).map(|n| format!("x{n}(); ")).collect();
    let source = format!("fn a() {{ big(); }}\nfn big() {{ {std}leaf(); }}\nfn leaf() {{}}\n");
    let mut spec = Spec::new(&[("a.rs", &source)]);
    spec.bindings = (0..250).map(|nth| bind_external("big", nth)).collect();
    spec.bindings.push(bind("big", 250, "leaf"));
    spec.bindings.push(bind("a", 0, "big"));
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let leaf = syntax_id(publisher.native(), "leaf");

    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert!(!view.truncated, "{:?}", view.warnings);
    assert_eq!(counts(&view.steps), (252, 2));
    assert_eq!(view.steps[0].children.len(), 251);
    assert_eq!(
        view.steps[0].children[250].target.as_deref(),
        Some(leaf.as_str())
    );
    assert_eq!((view.incidental_steps, view.hidden_steps), (250, 250));

    let all = publisher
        .store
        .sequence_at(&seed, pin, true)
        .unwrap()
        .unwrap();
    assert_eq!(counts(&all.steps), (1, 1));
    assert!(all.steps[0].children.is_empty());
    assert_eq!(
        all.truncated_by,
        [trellis::behavior::SequenceLimit::VisibleSteps]
    );
    assert!(
        all.warnings
            .iter()
            .any(|w| w == "Sequence step limit reached; the body of big is not expanded.")
    );
}

/// #541 review: Rust chain groups are steps too, so grouping 600 incidental three-call
/// chains (1800 calls) stops at the work bound instead of retaining 2400 steps; nothing
/// measured is lost.
#[test]
fn sequence_chain_groups_count_toward_the_work_bound() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let chains: String = (0..600).map(|n| format!("obj{n}.a().b().c();")).collect();
    let source = format!("fn run() {{ {chains} }}\n");
    let mut spec = Spec::new(&[("a.rs", &source)]);
    spec.bindings = (0..1800).map(|nth| bind_external("run", nth)).collect();
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "run");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert_eq!(counts(&view.steps), (2000, 0));
    assert_eq!(
        sequence_steps(&view.steps).len(),
        1800,
        "every call is kept"
    );
    assert_eq!(
        view.steps
            .iter()
            .filter(|step| step.kind == "group")
            .count(),
        200
    );
    assert!(
        view.warnings
            .iter()
            .any(|w| w.contains("Chain grouping traversal truncated"))
    );
}

/// #536: hidden steps use no visible room but still count toward the hard work bound, so
/// hostile input stays bounded: a seed is built to at most 2000 steps, and a callee body
/// that would take the view past them is not expanded.
#[test]
fn sequence_work_bound_holds_for_hidden_steps() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let calls = |n: usize| (0..n).map(|n| format!("x{n}(); ")).collect::<String>();
    let source = format!(
        "fn huge() {{ {} }}\nfn a() {{ big(); }}\nfn big() {{ {} }}\n",
        calls(2500),
        calls(2000)
    );
    let mut spec = Spec::new(&[("a.rs", &source)]);
    spec.bindings = (0..2500).map(|nth| bind_external("huge", nth)).collect();
    spec.bindings
        .extend((0..2000).map(|nth| bind_external("big", nth)));
    spec.bindings.push(bind("a", 0, "big"));
    let pin = publisher.publish(&spec);

    let huge = syntax_id(publisher.native(), "huge");
    let view = publisher
        .store
        .sequence_at(&huge, pin, false)
        .unwrap()
        .unwrap();
    assert_eq!(counts(&view.steps), (2000, 0));
    assert_eq!(
        view.truncated_by,
        [trellis::behavior::SequenceLimit::WorkSteps]
    );
    assert_eq!(view.incidental_steps, 2000);

    let a = syntax_id(publisher.native(), "a");
    let view = publisher
        .store
        .sequence_at(&a, pin, false)
        .unwrap()
        .unwrap();
    let steps = sequence_steps(&view.steps);
    assert_eq!(steps.len(), 1);
    assert!(
        steps[0].children.is_empty(),
        "1 + 2000 steps exceed the work bound"
    );
    assert_eq!(
        view.truncated_by,
        [trellis::behavior::SequenceLimit::WorkSteps]
    );
    assert!(
        view.warnings
            .iter()
            .any(|w| w == "Sequence work limit reached; the body of big is not expanded.")
    );
}

/// #522: a step the syntax adapter already hid (a standalone `console.log`) that is also
/// bound external is incidental once, not hidden twice.
#[test]
fn sequence_incidental_step_already_hidden_is_counted_once() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[(
        "a.js",
        "function a() { console.log(1); b(); }\nfunction b() {}\n",
    )]);
    spec.languages = vec![s::Language::Javascript];
    spec.bindings = vec![
        Bind {
            resolution: s::Resolution::External,
            dispatch: Dispatch::Unknown,
            ..bind("a", 0, "b")
        },
        bind("a", 1, "b"),
    ];
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    let steps = sequence_steps(&view.steps);
    assert!(steps[0].hidden);
    assert_eq!(
        steps[0].call_evidence.as_ref().unwrap().boundary_reason,
        BoundaryReason::External
    );
    assert_eq!((view.hidden_steps, view.incidental_steps), (1, 1));
}

#[test]
fn sequence_source_hints_do_not_consume_resolved_lane_budget() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let calls: String = (0..21)
        .map(|n| format!("const v{n}={{}}; v{n}.go();\n"))
        .collect();
    let source = format!("function a() {{ {calls} }}\nfunction b() {{}}\n");
    let mut spec = Spec::new(&[("a.js", &source)]);
    spec.languages = vec![s::Language::Javascript];
    spec.bindings = (0..21).map(|nth| bind("a", nth, "b")).collect();
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "a");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert_eq!(view.participants.len(), 2);
    assert!(
        sequence_steps(&view.steps)
            .iter()
            .all(|step| step.target.is_some())
    );
}

#[test]
fn sequence_participant_limit_keeps_evidence_without_inventing_target() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let source = fanout_source(21);
    let mut spec = Spec::new(&[("a.rs", &source)]);
    spec.bindings = (0..21)
        .map(|nth| bind("root", nth, &format!("f{nth}")))
        .collect();
    let pin = publisher.publish(&spec);
    let seed = syntax_id(publisher.native(), "root");
    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    assert_eq!(view.participants.len(), 20);
    assert!(view.truncated);
    let steps = sequence_steps(&view.steps);
    assert_eq!(steps.len(), 21);
    assert!(steps.iter().any(|step| {
        let evidence = step.call_evidence.as_ref().unwrap();
        evidence.boundary_reason == BoundaryReason::NodeLimit
            && step.target.is_none()
            && evidence.resolution == Some(s::Resolution::Resolved)
    }));
}

#[test]
fn sequence_corrupt_binding_fails_without_partial_result() {
    let (state, root) = workspace();
    let (pin, seed) = {
        let mut publisher = Publisher::open(state.path(), root.path());
        let mut spec = Spec::new(&[("a.rs", CHAIN)]);
        spec.bindings = vec![bind("a", 0, "b")];
        let pin = publisher.publish(&spec);
        (pin, syntax_id(publisher.native(), "a"))
    };
    sql(
        state.path(),
        "PRAGMA ignore_check_constraints=ON; UPDATE semantic_provenance SET freshness='bogus'",
    );
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let error = store.sequence_at(&seed, pin, false).unwrap_err();
    assert!(
        error.to_string().contains("incompatible_index"),
        "{error:#}"
    );
}

#[test]
fn published_freshness_is_trusted_not_recomputed() {
    let (state, root) = workspace();
    let root_id = {
        let mut publisher = Publisher::open(state.path(), root.path());
        let mut spec = Spec::new(&[("a.rs", CHAIN)]);
        spec.bindings = vec![bind("a", 0, "b")];
        publisher.publish(&spec);
        let id = syntax_id(publisher.native(), "a");
        let fresh = result(run(&publisher.store, &query(&id, true)).unwrap());
        assert_eq!(fresh.edges[0].visit, Visit::New);
        id
    };
    sql(
        state.path(),
        "UPDATE semantic_provenance SET freshness='possiblyStale' WHERE freshness='fresh'",
    );
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let result = result(run(&store, &query(&root_id, true)).unwrap());
    assert_eq!(result.edges[0].visit, Visit::Boundary);
    assert_eq!(result.edges[0].boundary_reason, BoundaryReason::Stale);
    assert!(warning_keys(&result).contains(&(WarningCode::StaleEvidence, None)));
}

#[test]
fn failed_refresh_returns_labelled_history_and_no_old_bindings() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", "fn a() { b(); }\nfn b() {}\n")]);
    spec.bindings = vec![bind("a", 0, "b")];
    spec.declaration_bindings = vec!["a".into()];
    publisher.publish(&spec);
    let r1_revision = publisher.native().revision.id.clone();
    let mut spec = Spec::new(&[("a.rs", "fn a() { b(); b(); }\nfn b() {}\n")]);
    spec.states.insert("a.rs".into(), CoverageState::Failed);
    publisher.publish(&spec);
    let native = publisher.native();
    let result = result(run(&publisher.store, &query(&syntax_id(native, "a"), true)).unwrap());
    assert_eq!(result.edges.len(), 2);
    assert!(result.edges.iter().all(|e| e.binding.is_none()
        && e.visit == Visit::Boundary
        && e.boundary_reason == BoundaryReason::MissingEvidence));
    let producer_rows: Vec<_> = result
        .coverage
        .iter()
        .filter(|c| c.producer_id == graph_fixtures::PRODUCER)
        .collect();
    assert_eq!(producer_rows.len(), 2);
    assert!(
        producer_rows
            .iter()
            .any(|c| c.revision_id == native.revision.id && c.state == CoverageState::Failed)
    );
    assert!(
        producer_rows
            .iter()
            .any(|c| c.revision_id == r1_revision && c.state == CoverageState::Complete)
    );
    let history: Vec<_> = result
        .provenance
        .iter()
        .filter(|p| p.revision_id == r1_revision)
        .collect();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, "decl-a");
    assert_eq!(history[0].freshness, s::Freshness::Stale);
    assert_eq!(
        warning_keys(&result),
        vec![
            (WarningCode::CoverageIncomplete, None),
            (WarningCode::StaleEvidence, Some("decl-a".to_string())),
        ]
    );
    assert!(result.partial);
}

#[test]
fn corrupt_stored_row_is_typed_without_partial_result() {
    let (state, root) = workspace();
    let root_id = {
        let mut publisher = Publisher::open(state.path(), root.path());
        let mut spec = Spec::new(&[("a.rs", CHAIN)]);
        spec.bindings = vec![bind("a", 0, "b")];
        publisher.publish(&spec);
        syntax_id(publisher.native(), "a")
    };
    sql(
        state.path(),
        "PRAGMA ignore_check_constraints=ON; UPDATE semantic_provenance SET freshness='bogus'",
    );
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    assert!(run(&store, &query(&root_id, true)).is_err());
}

#[test]
fn in_scope_missing_coverage_row_is_typed_corruption() {
    for sequence in [false, true] {
        let (state, root) = workspace();
        let (root_id, pin) = {
            let mut publisher = Publisher::open(state.path(), root.path());
            let pin = publisher.publish(&Spec::new(&[("a.rs", CHAIN)]));
            (syntax_id(publisher.native(), "a"), pin)
        };
        sql(
            state.path(),
            "PRAGMA foreign_keys=OFF; DELETE FROM semantic_coverage WHERE path='a.rs'",
        );
        let store = Store::open_for_tests(state.path(), root.path()).unwrap();
        let error = if sequence {
            store.sequence_at(&root_id, pin, false).unwrap_err()
        } else {
            run(&store, &query(&root_id, true)).err().unwrap()
        };
        assert!(
            error
                .to_string()
                .contains("missing producer coverage tuple"),
            "{error:#}"
        );
    }
}

#[test]
fn out_of_scope_root_with_producer_is_missing_evidence() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let spec = Spec::new(&[
        ("src/lib.rs", "fn lib() {}\n"),
        (
            "web/a.js",
            "function r() { f(); g(); }\nfunction f() {}\nfunction g() {}\n",
        ),
    ]);
    publisher.publish(&spec);
    let root_id = syntax_id(publisher.native(), "r");
    let result = result(run(&publisher.store, &query(&root_id, true)).unwrap());
    assert_eq!(result.edges.len(), 2);
    assert!(
        result
            .edges
            .iter()
            .all(|e| e.boundary_reason == BoundaryReason::MissingEvidence)
    );
    assert!(
        result
            .coverage
            .iter()
            .all(|c| c.producer_id != graph_fixtures::PRODUCER)
    );
    assert!(
        result
            .coverage
            .iter()
            .all(|c| c.document_path == "web/a.js")
    );
    drop(result);
    let again = result_of(run(&publisher.store, &query(&root_id, true)));
    assert_eq!(again.edges.len(), 2);
}

fn result_of(answer: Result<GraphAnswer>) -> GraphResult {
    result(answer.unwrap())
}

const CONTRADICTED: &str = "fn a() { b(); }\nfn b() {}\nfn c() {}\nfn d() {}\n";

/// #466: contradictory evidence at one call site, published the way the importer does it: one
/// ambiguous row per provenance, all sharing `call_id`, each carrying the full candidate union.
fn publish_contradicted(publisher: &mut Publisher) -> trellis::model::IndexPin {
    let mut spec = Spec::new(&[("a.rs", CONTRADICTED)]);
    spec.bindings = [("amb-3", "d"), ("amb-1", "b"), ("amb-2", "c")]
        .into_iter()
        .map(|(provenance, target)| Bind {
            resolution: s::Resolution::Ambiguous,
            provenance: provenance.into(),
            ..bind("a", 0, target)
        })
        .collect();
    publisher.publish(&spec)
}

#[test]
fn ambiguous_call_rows_answer_graph_sequence_and_navigation_without_recovery() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let pin = publish_contradicted(&mut publisher);
    let seed = syntax_id(publisher.native(), "a");
    let names = |ids: &[&str]| -> Vec<String> {
        ids.iter()
            .map(|n| syntax_id(publisher.native(), n))
            .collect()
    };
    let expected_internal = names(&["b", "c", "d"]);

    let graph = result(run(&publisher.store, &query(&seed, true)).unwrap());
    assert_eq!(graph.edges.len(), 1);
    let edge = &graph.edges[0];
    assert_eq!(edge.visit, Visit::Boundary);
    assert_eq!(edge.boundary_reason, BoundaryReason::Ambiguous);
    let binding = edge.binding.as_ref().expect("ambiguous binding");
    assert_eq!(binding.resolution, s::Resolution::Ambiguous);
    assert_eq!(binding.provenance_id, "amb-1");
    assert_eq!(
        binding.candidates.len(),
        4,
        "three internal targets and one external"
    );
    let mut internal: Vec<_> = binding
        .candidates
        .iter()
        .filter_map(|t| match t {
            s::Target::Internal { syntax_id, .. } => Some(syntax_id.clone()),
            s::Target::External { .. } => None,
        })
        .collect();
    internal.sort();
    let mut expected = expected_internal.clone();
    expected.sort();
    assert_eq!(internal, expected);
    let proofs: Vec<_> = graph
        .provenance
        .iter()
        .filter(|p| p.producer_id == graph_fixtures::PRODUCER)
        .map(|p| p.id.as_str())
        .collect();
    assert_eq!(proofs, vec!["amb-1", "amb-2", "amb-3"]);
    assert_eq!(
        warning_keys(&graph),
        vec![(WarningCode::BindingAmbiguous, Some("amb-1".into()))]
    );

    let view = publisher
        .store
        .sequence_at(&seed, pin, false)
        .unwrap()
        .unwrap();
    let steps = sequence_steps(&view.steps);
    assert_eq!(steps.len(), 1);
    let evidence = steps[0].call_evidence.as_ref().unwrap();
    assert!(steps[0].target.is_none());
    assert_eq!(evidence.boundary_reason, BoundaryReason::Ambiguous);
    assert_eq!(evidence.resolution, Some(s::Resolution::Ambiguous));
    assert_eq!(evidence.provenance_id.as_deref(), Some("amb-1"));
    assert_eq!(view.semantic_warnings, graph.warnings);

    let navigation = publisher
        .store
        .navigation_at(
            &serde_json::from_value(serde_json::json!({"expectedRevision":pin,"path":"a.rs",
                "line":1,"semanticProducerId":graph_fixtures::PRODUCER}))
            .unwrap(),
        )
        .unwrap();
    assert!(navigation.targets.iter().all(|t| t.reason != "call"));
    assert!(!publisher.store.recovery_pending_for_tests());
}

#[test]
fn non_ambiguous_multi_match_for_one_call_stays_typed_corruption() {
    for case in ["ambiguous+unresolved", "duplicate resolved"] {
        let (state, root) = workspace();
        let (pin, seed) = {
            let mut publisher = Publisher::open(state.path(), root.path());
            let pin = if case == "duplicate resolved" {
                let mut spec = Spec::new(&[("a.rs", CONTRADICTED)]);
                spec.bindings = vec![bind("a", 0, "b")];
                publisher.publish(&spec)
            } else {
                publish_contradicted(&mut publisher)
            };
            (pin, syntax_id(publisher.native(), "a"))
        };
        sql(
            state.path(),
            if case == "duplicate resolved" {
                "CREATE TEMP TABLE copy AS SELECT * FROM semantic_call_bindings; \
                 UPDATE copy SET binding_id=binding_id+1000, ordinal=ordinal+1; \
                 INSERT INTO semantic_call_bindings SELECT * FROM copy;"
            } else {
                "PRAGMA foreign_keys=OFF; \
                 DELETE FROM semantic_call_candidates WHERE binding_id IN \
                   (SELECT binding_id FROM semantic_call_bindings WHERE provenance_id='amb-2'); \
                 UPDATE semantic_call_bindings SET resolution='unresolved' \
                   WHERE provenance_id='amb-2';"
            },
        );
        for path in ["graph", "sequence"] {
            let store = Store::open_for_tests(state.path(), root.path()).unwrap();
            let error = if path == "sequence" {
                store.sequence_at(&seed, pin, false).unwrap_err()
            } else {
                run(&store, &query(&seed, true)).err().unwrap()
            };
            assert!(
                error
                    .to_string()
                    .contains("multiple call bindings match one call"),
                "{case} {path}: {error:#}"
            );
            // The MCP and HTTP boundaries report a graph read failure; sequence reports itself.
            assert_eq!(
                store.recovery_pending_for_tests(),
                path == "sequence",
                "{case} {path}"
            );
        }
    }
}

#[test]
fn high_fanout_syntax_only_store() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    publisher.publish(&Spec::syntax_only(&[("a.rs", &fanout_source(600))]));
    let result = result(
        run(
            &publisher.store,
            &query(&syntax_id(publisher.native(), "root"), false),
        )
        .unwrap(),
    );
    assert_eq!(result.nodes.len(), 1);
    assert_eq!(result.edges.len(), 500);
    assert!(
        result
            .edges
            .iter()
            .all(|e| e.boundary_reason == BoundaryReason::MissingEvidence)
    );
    assert_eq!(result.frontier.len(), 1);
    assert_eq!(result.frontier[0].reason, FrontierReason::CallLimit);
    assert_eq!(result.frontier[0].next_ordinal, None);
    assert_eq!(result.omitted.call_limit, 100);
    assert_eq!(result.frontier[0].omitted_calls, 100);
    assert!(result.truncated && result.partial);
    assert_eq!(warning_keys(&result), vec![(WarningCode::SyntaxOnly, None)]);
}

#[test]
fn shared_proof_duplicates_group_but_distinct_proofs_do_not() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", "fn a() { b(); b(); b(); }\nfn b() {}\n")]);
    spec.bindings = vec![
        bind("a", 0, "b"),
        Bind {
            provenance: "call-a-0".into(),
            ..bind("a", 1, "b")
        },
        bind("a", 2, "b"),
    ];
    publisher.publish(&spec);
    let native = publisher.native();
    let root_id = syntax_id(native, "a");
    let calls = graph_fixtures::calls_of(native, "a");
    let result = result(run(&publisher.store, &query(&root_id, true)).unwrap());
    assert_eq!(result.nodes.len(), 2);
    assert_eq!(result.edges.len(), 2);
    assert_eq!(result.edges[0].call_site_count, 2);
    assert_eq!(result.edges[0].call, *calls[0]);
    assert_eq!(
        result.edges[0].binding.as_ref().unwrap().call_id.as_ref(),
        Some(&calls[0].id)
    );
    assert_eq!(result.edges[0].visit, Visit::New);
    assert_eq!(result.edges[1].call_site_count, 1);
    assert_eq!(result.edges[1].call, *calls[2]);
    assert_eq!(result.edges[1].visit, Visit::Seen);
    assert!(!result.partial && !result.truncated);
    assert!(result.frontier.is_empty());
    assert_eq!(
        result.omitted.filtered
            + result.omitted.call_limit
            + result.omitted.depth
            + result.omitted.response_bytes,
        0
    );
}

#[test]
fn internal_priority_preserves_closure_and_node_limit_does_not_demote() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[(
        "a.rs",
        "fn a() { outside(); b(); unknown(); }\nfn b() { c(); }\nfn c() {}\n",
    )]);
    spec.bindings = vec![
        Bind {
            resolution: s::Resolution::External,
            target: None,
            ..bind("a", 0, "unused")
        },
        bind("a", 1, "b"),
        bind("b", 0, "c"),
    ];
    publisher.publish(&spec);
    let native = publisher.native();
    let root_id = syntax_id(native, "a");
    let b_id = syntax_id(native, "b");
    let c_id = syntax_id(native, "c");
    let included = result(
        run(
            &publisher.store,
            &GraphQuery {
                max_calls: 2,
                ..query(&root_id, true)
            },
        )
        .unwrap(),
    );
    assert_eq!(
        included
            .edges
            .iter()
            .map(|edge| edge.to.as_deref())
            .collect::<Vec<_>>(),
        vec![Some(b_id.as_str()), Some(c_id.as_str())]
    );
    assert_eq!(
        included
            .nodes
            .iter()
            .map(|node| (&node.declaration.syntax_id, node.depth))
            .collect::<Vec<_>>(),
        vec![(&root_id, 0), (&b_id, 1), (&c_id, 2)]
    );
    assert_eq!(included.omitted.call_limit, 2);
    assert!(
        included
            .frontier
            .iter()
            .all(|frontier| frontier.node_id == root_id
                && frontier.reason == FrontierReason::CallLimit
                && frontier.next_ordinal.is_none())
    );

    let refused = result(
        run(
            &publisher.store,
            &GraphQuery {
                max_calls: 1,
                max_nodes: 1,
                ..query(&root_id, true)
            },
        )
        .unwrap(),
    );
    assert_eq!(refused.edges.len(), 1);
    assert_eq!(refused.edges[0].call.spelling.as_deref(), Some("b"));
    assert_eq!(refused.edges[0].boundary_reason, BoundaryReason::NodeLimit);
    assert_eq!(refused.omitted.call_limit, 2);
    assert_eq!(refused.frontier[0].reason, FrontierReason::NodeLimit);
    assert_eq!(refused.frontier[0].target_id.as_ref(), Some(&b_id));
    assert_eq!(refused.frontier[0].omitted_calls, 0);
}

#[test]
fn duplicate_name_store() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        ("a.rs", "fn a() { dup(); }\nfn dup() {}\n"),
        ("b.rs", "fn dup() {}\n"),
    ]);
    spec.bindings = vec![bind("a", 0, "dup")];
    publisher.publish(&spec);
    let native = publisher.native();
    let a_id = syntax_id(native, "a");
    let dup_in_a = native
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("dup") && d.document.path == "a.rs")
        .unwrap()
        .syntax_id
        .clone();
    let dup_in_b = native
        .declarations
        .iter()
        .find(|d| d.name.as_deref() == Some("dup") && d.document.path == "b.rs")
        .unwrap()
        .syntax_id
        .clone();
    assert_ne!(dup_in_a, dup_in_b);
    let result = result(run(&publisher.store, &query(&a_id, true)).unwrap());
    let ids: Vec<_> = result
        .nodes
        .iter()
        .map(|n| n.declaration.syntax_id.clone())
        .collect();
    assert_eq!(result.edges.len(), 1);
    let target = result.edges[0].to.clone().unwrap();
    // The fixture resolves the name to its first declaration; only that duplicate expands.
    assert!(target == dup_in_a || target == dup_in_b);
    let other = if target == dup_in_a {
        &dup_in_b
    } else {
        &dup_in_a
    };
    assert_eq!(ids, vec![a_id, target]);
    assert!(!ids.contains(other));
}

#[test]
fn check_closure_runs_for_nodes_and_local_calls_and_its_error_aborts() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", CHAIN)]);
    spec.bindings = vec![bind("a", 0, "b"), bind("b", 0, "c")];
    publisher.publish(&spec);
    let root_id = syntax_id(publisher.native(), "a");
    let response = publisher.store.evidence_response().unwrap();
    let mut calls = 0;
    let answer = response
        .graph_query(
            &GraphQuery {
                depth: 5,
                ..query(&root_id, true)
            },
            None,
            &mut || {
                calls += 1;
                Ok(())
            },
        )
        .unwrap();
    assert_eq!(calls, 5); // Three nodes and two scanned local calls.
    assert_eq!(result(answer).nodes.len(), 3);

    let mut calls = 0;
    let error = response
        .graph_query(
            &GraphQuery {
                depth: 5,
                ..query(&root_id, true)
            },
            None,
            &mut || {
                calls += 1;
                if calls == 2 {
                    anyhow::bail!("deadline_exceeded: test abort");
                }
                Ok(())
            },
        )
        .err()
        .unwrap();
    assert_eq!(error.to_string(), "deadline_exceeded: test abort");
    assert_eq!(calls, 2);
}

/// Runs in a child process with the phase-hook environment; see the parent test below.
#[test]
#[ignore = "runs only as the child of call_graph_node_hook_fires_once_per_dequeued_node"]
fn call_graph_node_hook_child() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", CHAIN)]);
    spec.bindings = vec![bind("a", 0, "b"), bind("b", 0, "c")];
    publisher.publish(&spec);
    let root_id = syntax_id(publisher.native(), "a");
    let answer = run(
        &publisher.store,
        &GraphQuery {
            depth: 5,
            ..query(&root_id, true)
        },
    )
    .unwrap();
    assert_eq!(result(answer).nodes.len(), 3);
}

#[test]
fn call_graph_node_hook_fires_once_per_dequeued_node() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("phase.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let child_test = child_test_name("call_graph_node_hook_child");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            child_test.as_str(),
            "--ignored",
            "--test-threads=1",
        ])
        .env("TRELLIS_TEST_MCP_PHASE", "call_graph_node")
        .env("TRELLIS_TEST_MCP_PHASE_SOCKET", &socket)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut seen = 0;
    while seen < 3 {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                while stream.read_exact(&mut byte).is_ok() && byte[0] != b'\n' {
                    line.push(byte[0]);
                }
                assert_eq!(line, b"call_graph_node");
                stream.write_all(&[1]).unwrap();
                seen += 1;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "child never reached the hook");
                assert!(child.try_wait().unwrap().is_none(), "child exited early");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    }
    assert!(child.wait().unwrap().success());
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

#[test]
fn frozen_answer_prefixes_preserve_ancestor_closure_without_sql() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        ("a.rs", "fn a() { b(); }\n"),
        ("b.rs", "fn b() { c(); }\n"),
        ("c.rs", "fn c() { d(); }\nfn d() {}\n"),
    ]);
    spec.bindings = vec![bind("a", 0, "b"), bind("b", 0, "c"), bind("c", 0, "d")];
    publisher.publish(&spec);
    let native = publisher.native();
    let root_id = syntax_id(native, "a");
    let b_id = syntax_id(native, "b");
    let c_id = syntax_id(native, "c");
    let mut answer = run(&publisher.store, &query(&root_id, true)).unwrap();
    let full = result(answer.clone());
    assert_eq!(full.edges.len(), 2);
    assert_eq!(full.omitted.depth, 1);
    drop(publisher);
    std::fs::remove_file(root.path().join("a.rs")).unwrap();
    std::fs::remove_file(root.path().join("b.rs")).unwrap();
    std::fs::remove_file(root.path().join("c.rs")).unwrap();

    for count in 0..=full.edges.len() {
        let prefix = answer.prefix(count).unwrap();
        assert_eq!(prefix, answer.prefix(count).unwrap(), "count={count}");
        assert_eq!(prefix.request, full.request);
        assert_eq!(prefix.edges, full.edges[..count]);
        let ids: Vec<_> = prefix
            .nodes
            .iter()
            .map(|node| node.declaration.syntax_id.as_str())
            .collect();
        let expected_ids: Vec<_> = match count {
            0 => vec![root_id.as_str()],
            1 => vec![root_id.as_str(), b_id.as_str()],
            _ => vec![root_id.as_str(), b_id.as_str(), c_id.as_str()],
        };
        assert_eq!(ids, expected_ids);
        assert!(
            prefix
                .edges
                .iter()
                .all(|edge| ids.contains(&edge.from.as_str())
                    && edge.to.as_deref().is_none_or(|to| ids.contains(&to)))
        );
        assert!(
            prefix
                .frontier
                .iter()
                .all(|frontier| ids.contains(&frontier.node_id.as_str()))
        );
        assert_eq!(
            prefix.omitted.response_bytes,
            u64::from(count < full.edges.len())
        );
        assert_eq!(prefix.omitted.depth, u64::from(count == full.edges.len()));
        assert_eq!(prefix.omitted.call_limit, 0);
        assert_eq!(
            prefix
                .provenance
                .iter()
                .filter(|proof| proof.producer_id == graph_fixtures::PRODUCER)
                .count(),
            count
        );
        assert_eq!(
            prefix
                .coverage
                .iter()
                .filter(|row| row.producer_id == graph_fixtures::PRODUCER)
                .count(),
            count + 1
        );
    }
    assert_eq!(answer.prefix(full.edges.len()).unwrap(), full);
    let unread = "sid:v1:ffffffffffffffffffffffffffffffff";
    assert!(answer.facts.locate(unread).is_err());
}

fn cli(home: &Path, args: &[&str]) -> serde_json::Value {
    let mut cmd = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    let output = cmd
        .env("HOME", home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("daemon.pid"))
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(std::process::Stdio::null())
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

/// A real SCIP capture of `run() -> target()` at r1, imported as a batch against the live r1 native revision.
fn import_scip_capture(home: &Path, root: &Path, index_db: &Path) {
    let mut fixture = BatchFixture::from_published(index_db, root).reference("scip-typescript");
    let a = DocSpec::new("src/A.js")
        .occurrence(&[0, 9, 12], &[], "pkg run.", 1)
        .occurrence(&[0, 16, 22], &[0, 16, 24], "pkg target.", 0)
        .symbol("pkg run.", 26, &[]);
    let b = DocSpec::new("src/B.js")
        .occurrence(&[0, 9, 15], &[], "pkg target.", 1)
        .symbol("pkg target.", 26, &[]);
    fixture.artifact("javascript", vec![a, b]);
    let (descriptor, digest) = fixture.write(&root.join(".trellis-semantic-batch"));
    cli(
        home,
        &[
            "semantic",
            "import",
            "--workspace",
            root.to_str().unwrap(),
            "--batch",
            descriptor.to_str().unwrap(),
            "--batch-sha256",
            &digest,
        ],
    );
}

#[test]
fn rejoined_binding_on_edited_workspace_is_a_stale_boundary_with_its_binding() {
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::Builder::new()
        .prefix("graph-rejoin-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let root = temp.path().join("checkout");
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join(".gitignore"), ".trellis-semantic-batch*/\n").unwrap();
    std::fs::write(root.join("src/A.js"), "function run(){ target(); }\n").unwrap();
    std::fs::write(root.join("src/B.js"), "function target(){ return 1; }\n").unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let root_string = root.to_str().unwrap();
    let _r1: trellis::model::IndexPin = serde_json::from_value(
        cli(&home, &["index", "--workspace", root_string])["publishedRevision"].clone(),
    )
    .unwrap();
    let (cache, data) = if cfg!(target_os = "macos") {
        (
            home.join("Library/Caches/dev.squashmerge.trellis"),
            home.join("Library/Application Support/dev.squashmerge.trellis"),
        )
    } else {
        (
            home.join(".cache/trellis"),
            home.join(".local/share/trellis"),
        )
    };
    let open = || {
        Store::open(
            TopologyRoots::isolated_for_tests(cache.clone(), data.clone()),
            WorkspaceIdentity::discover(Some(&root), &root).unwrap(),
        )
        .unwrap()
    };
    let index_db = TopologyRoots::isolated_for_tests(cache.clone(), data.clone())
        .index_db(&WorkspaceIdentity::discover(Some(&root), &root).unwrap());
    import_scip_capture(&home, &root, &index_db);

    std::fs::write(root.join("src/B.js"), "function target(){ return 2; }\n").unwrap();
    let r2: trellis::model::IndexPin = serde_json::from_value(
        cli(&home, &["index", "--workspace", root_string])["publishedRevision"].clone(),
    )
    .unwrap();
    let store = open();
    let (r2_native, _) = store.retained_native_snapshot_at(r2).unwrap();
    let run_id = syntax_id(&r2_native, "run");
    let producer = store.semantic_at(r2).unwrap().producers[0].id.clone();
    let mut q = query(&run_id, true);
    q.semantic_producer_id = Some(producer);
    let result = result(run(&store, &q).unwrap());

    assert_eq!(result.edges.len(), 1);
    let edge = &result.edges[0];
    assert_eq!(edge.visit, Visit::Boundary);
    assert_eq!(edge.boundary_reason, BoundaryReason::Stale);
    let binding = edge.binding.as_ref().expect("rejoined binding is attached");
    let proof = result
        .provenance
        .iter()
        .find(|p| p.id == binding.provenance_id)
        .expect("binding provenance is returned");
    assert!(proof.derived_from.is_some());
    assert_eq!(proof.freshness, s::Freshness::PossiblyStale);
    assert!(warning_keys(&result).contains(&(WarningCode::StaleEvidence, None)));
    assert!(result.partial);
}

/// A child test's libtest name in the grouped test binary, which names this
/// module's tests `<module>::<test>`.
fn child_test_name(test: &str) -> String {
    format!("{}::{test}", module_path!().split_once("::").unwrap().1)
}
