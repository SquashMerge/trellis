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

#[path = "common/graph_fixtures.rs"]
mod graph_fixtures;
use graph_fixtures::{Bind, Publisher, Spec, bind, fanout_source, syntax_id};

const CHAIN: &str = "fn a() { b(); }\nfn b() { c(); }\nfn c() {}\n";

fn query(root: &str, producer: bool) -> GraphQuery {
    GraphQuery {
        root_syntax_id: root.into(),
        semantic_producer_id: producer.then(|| graph_fixtures::PRODUCER.into()),
        depth: 2,
        max_nodes: 150,
        max_calls: 500,
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

#[test]
fn query_view_never_traverses_semantic_bindings() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", CHAIN)]);
    spec.bindings = vec![bind("a", 0, "b"), bind("b", 0, "c")];
    publisher.publish(&spec);
    let seed = publisher
        .graph
        .as_ref()
        .unwrap()
        .nodes
        .iter()
        .find(|node| node.name == "a")
        .unwrap()
        .id
        .clone();
    let view: trellis::model::ViewQuery =
        serde_json::from_value(serde_json::json!({"seed": seed, "depth": 5})).unwrap();
    let view = publisher.store.query_view(&view).unwrap().unwrap();
    assert_eq!(view.nodes.len(), 1);
    assert_eq!(view.nodes[0].id, seed);
    assert!(!view.calls.is_empty());
}

/// IDs embed the checkout's root ID; rename them by order of first appearance.
fn normalize_ids(value: &mut serde_json::Value, seen: &mut Vec<String>) {
    match value {
        serde_json::Value::String(text)
            if text.starts_with("sid:v1:") || text.starts_with("occ:v2:") =>
        {
            let at = seen.iter().position(|id| id == text).unwrap_or_else(|| {
                seen.push(text.clone());
                seen.len() - 1
            });
            *text = format!("{}#{at}", &text[..7]);
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(|v| normalize_ids(v, seen)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|v| normalize_ids(v, seen)),
        _ => {}
    }
}

#[test]
fn sequence_view_unchanged_with_semantic_bindings() {
    let sequence = |semantic: bool| {
        let (state, root) = workspace();
        let mut publisher = Publisher::open(state.path(), root.path());
        let mut spec = if semantic {
            Spec::new(&[("a.rs", CHAIN)])
        } else {
            Spec::syntax_only(&[("a.rs", CHAIN)])
        };
        if semantic {
            spec.bindings = vec![bind("a", 0, "b")];
        }
        let pin = publisher.publish(&spec);
        let seed = publisher
            .graph
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|node| node.name == "a")
            .unwrap()
            .id
            .clone();
        let view = publisher
            .store
            .sequence_at(&seed, pin, false)
            .unwrap()
            .unwrap();
        assert!(!view.steps.is_empty());
        fn targetless(steps: &[trellis::behavior::SequenceStep]) -> bool {
            steps.iter().all(|step| {
                step.target.is_none()
                    && step.resolution.is_none()
                    && targetless(&step.children)
                    && targetless(&step.alternate)
            })
        }
        assert!(targetless(&view.steps));
        let mut value = serde_json::to_value(&view).unwrap();
        value["revision"] = serde_json::Value::Null;
        normalize_ids(&mut value, &mut Vec::new());
        value
    };
    assert_eq!(sequence(true), sequence(false));
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
    let (state, root) = workspace();
    let root_id = {
        let mut publisher = Publisher::open(state.path(), root.path());
        publisher.publish(&Spec::new(&[("a.rs", CHAIN)]));
        syntax_id(publisher.native(), "a")
    };
    sql(
        state.path(),
        "PRAGMA foreign_keys=OFF; DELETE FROM semantic_coverage WHERE path='a.rs'",
    );
    let store = Store::open_for_tests(state.path(), root.path()).unwrap();
    let error = run(&store, &query(&root_id, true)).err().unwrap();
    assert!(
        error
            .to_string()
            .contains("missing producer coverage tuple"),
        "{error:#}"
    );
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
    assert_eq!(result.frontier[0].next_ordinal, Some(500));
    assert_eq!(result.frontier[0].omitted_calls, 100);
    assert!(result.truncated && result.partial);
    assert_eq!(warning_keys(&result), vec![(WarningCode::SyntaxOnly, None)]);
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
fn check_closure_runs_once_per_dequeued_node_and_its_error_aborts() {
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
    assert_eq!(calls, 3);
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
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "call_graph_node_hook_child",
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
fn answer_facts_replay_every_reduced_max_calls_without_sql() {
    let (state, root) = workspace();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[("a.rs", &fanout_source(4))]);
    spec.states.insert("a.rs".into(), CoverageState::Omitted);
    publisher.publish(&spec);
    let root_id = syntax_id(publisher.native(), "root");
    let mut answer = run(&publisher.store, &query(&root_id, true)).unwrap();
    let edges = result(answer.clone()).edges.len();
    assert_eq!(edges, 4);
    let fresh: Vec<GraphResult> = (0..=edges as u64)
        .map(|k| {
            result(
                run(
                    &publisher.store,
                    &GraphQuery {
                        max_calls: k,
                        ..query(&root_id, true)
                    },
                )
                .unwrap(),
            )
        })
        .collect();
    drop(publisher);
    for (k, expected) in fresh.iter().enumerate() {
        assert_eq!(&answer.reevaluate(k as u64).unwrap(), expected, "k={k}");
    }
    let unread = "sid:v1:ffffffffffffffffffffffffffffffff";
    assert!(answer.facts.locate(unread).is_err());
}
