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

/// A real SCIP capture of `run() -> target()` at r1, joined to a retained r1 native snapshot.
fn import_scip_capture(home: &Path, root: &Path, native: &trellis::native_evidence::Artifact) {
    use protobuf::{EnumOrUnknown, Message};
    use trellis::native_ids;
    let a = "function run(){ target(); }\n";
    let b = "function target(){ return 1; }\n";
    let mut binary = scip::types::Index::new();
    for (path, text) in [("src/A.js", a), ("src/B.js", b)] {
        let mut doc = scip::types::Document::new();
        doc.relative_path = path.into();
        doc.language = "javascript".into();
        doc.position_encoding = EnumOrUnknown::from_i32(2);
        let start = text.find("target").unwrap();
        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = vec![0, start as i32, (start + 6) as i32];
        occurrence.symbol = "pkg target.".into();
        occurrence.symbol_roles = if path.ends_with("A.js") { 0 } else { 1 };
        occurrence.syntax_kind = EnumOrUnknown::from_i32(0);
        if path.ends_with("A.js") {
            occurrence.enclosing_range = vec![0, start as i32, (start + 8) as i32];
            let start = text.find("run").unwrap();
            let mut independent = scip::types::Occurrence::new();
            independent.range = vec![0, start as i32, (start + 3) as i32];
            independent.symbol = "pkg run.".into();
            independent.symbol_roles = 1;
            independent.syntax_kind = EnumOrUnknown::from_i32(0);
            doc.occurrences.push(independent);
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = "pkg run.".into();
            info.kind = EnumOrUnknown::from_i32(26);
            info.display_name = "run".into();
            doc.symbols.push(info);
        } else {
            let mut info = scip::types::SymbolInformation::new();
            info.symbol = "pkg target.".into();
            info.kind = EnumOrUnknown::from_i32(26);
            info.display_name = "target".into();
            doc.symbols.push(info);
        }
        doc.occurrences.push(occurrence);
        binary.documents.push(doc);
    }
    let bytes = binary.write_to_bytes().unwrap();
    std::fs::write(root.join("artifact.scip"), &bytes).unwrap();
    let profile: serde_json::Value = serde_json::from_str(include_str!(
        "../docs/semantic-evidence/hardware/mac-mini-m5-pro-v1.json"
    ))
    .unwrap();
    let producer = profile["semanticProducers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "scip-typescript")
        .unwrap();
    let documents: Vec<_> = native
        .revision
        .documents
        .iter()
        .map(|d| serde_json::json!({"language":d.key.language,"relativePath":d.key.path,"contentHash":d.content_hash}))
        .collect();
    let manifest_hash = native_ids::digest(
        b"",
        &native_ids::canonical(&serde_json::json!(
            native
                .revision
                .documents
                .iter()
                .map(|d| serde_json::json!({"document":d.key,"contentHash":d.content_hash}))
                .collect::<Vec<_>>()
        )),
    );
    let manifest = serde_json::json!({"formatVersion":1,"trustOrigin":"referenceProfile","completion":"complete","decoderVersion":"scip-0.10.0/CapturedScipFactV1/1","profileId":profile["profileId"],
      "producer":{"name":producer["name"],"version":producer["version"],"executableHash":producer["executableHash"],"positionEncoding":2},
      "sourceSet":native.source_set,"sourceRevision":native.revision.id,"sourceManifestHash":manifest_hash,
      "components":{"toolchainHash":native.revision.toolchain_hash,"configHash":native.revision.config_hash,"dependencyHash":native.revision.dependency_hash},
      "metadata":{"protocolVersion":0,"projectRoot":format!("file://{}",root.display()),"textDocumentEncoding":1},
      "sourceDocuments":documents,"documents":documents,"exclusions":[],
      "artifacts":[{"kind":"scipBinary","path":"artifact.scip","hash":native_ids::digest(b"",&bytes),"language":"javascript","documents":["src/A.js","src/B.js"]}]});
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    std::fs::write(root.join("manifest.json"), &manifest_bytes).unwrap();
    cli(
        home,
        &[
            "semantic",
            "import",
            "--workspace",
            root.to_str().unwrap(),
            "--scip",
            root.join("artifact.scip").to_str().unwrap(),
            "--manifest",
            root.join("manifest.json").to_str().unwrap(),
            "--profile",
            "mac-mini-m5-pro-v1",
            "--producer",
            "scip-typescript",
            "--manifest-sha256",
            &native_ids::digest(b"", &manifest_bytes),
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
    std::fs::write(root.join(".gitignore"), "artifact.scip\nmanifest.json\n").unwrap();
    std::fs::write(root.join("src/A.js"), "function run(){ target(); }\n").unwrap();
    std::fs::write(root.join("src/B.js"), "function target(){ return 1; }\n").unwrap();
    let root = std::fs::canonicalize(root).unwrap();
    let root_string = root.to_str().unwrap();
    let r1: trellis::model::IndexPin = serde_json::from_value(
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
    let (r1_native, _) = open().retained_native_snapshot_at(r1).unwrap();
    import_scip_capture(&home, &root, &r1_native);

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
