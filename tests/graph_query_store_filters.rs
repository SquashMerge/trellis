#![cfg(feature = "test-support")]
//! Graph filters use only published bindings and stored declaration roles.
use trellis::{
    graph_query::{BoundaryReason, FrontierReason, GraphQuery, GraphResult, Visit},
    native_evidence::DeclarationRole,
    semantic_evidence::Resolution,
};

#[path = "common/graph_fixtures.rs"]
mod graph_fixtures;
use graph_fixtures::{Bind, Publisher, Spec, bind, syntax_id};

fn query(root: &str, producer: bool, include_external: bool, include_tests: bool) -> GraphQuery {
    GraphQuery {
        root_syntax_id: root.into(),
        semantic_producer_id: producer.then(|| graph_fixtures::PRODUCER.into()),
        depth: 2,
        max_nodes: 150,
        max_calls: 500,
        include_external,
        include_tests,
    }
}

fn run(publisher: &Publisher, query: &GraphQuery) -> GraphResult {
    let answer = publisher
        .store
        .evidence_response()
        .unwrap()
        .graph_query(query, None, &mut || Ok(()))
        .unwrap();
    assert_eq!(answer.requested.include_external, query.include_external);
    assert_eq!(answer.requested.include_tests, query.include_tests);
    let result = answer.outcome.unwrap();
    assert_eq!(result.request, answer.requested);
    result
}

#[test]
fn published_external_filter_does_not_guess_unknown_calls_from_spelling() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[(
        "a.rs",
        "fn root() { outside(); std::mem::drop(1); }
",
    )]);
    spec.bindings = vec![Bind {
        resolution: Resolution::External,
        target: None,
        ..bind("root", 0, "unused")
    }];
    publisher.publish(&spec);
    let root_id = syntax_id(publisher.native(), "root");
    let filtered = run(&publisher, &query(&root_id, true, false, false));
    assert_eq!(filtered.edges.len(), 1);
    assert_eq!(filtered.edges[0].call.spelling.as_deref(), Some("drop"));
    assert!(filtered.edges[0].binding.is_none());
    assert_eq!(filtered.omitted.filtered, 1);
    assert!(filtered.partial);
    assert!(!filtered.truncated);
    assert_eq!(filtered.hints, vec!["includeExternal"]);
    let frontier = &filtered.frontier[0];
    assert_eq!(frontier.reason, FrontierReason::Filtered);
    assert_eq!(frontier.omitted_calls, 1);
    assert_eq!(
        (
            &frontier.call_id,
            &frontier.target_id,
            frontier.next_ordinal
        ),
        (&None, &None, None)
    );

    let included = run(&publisher, &query(&root_id, true, true, false));
    assert_eq!(included.edges.len(), 2);
    assert_eq!(included.edges[0].boundary_reason, BoundaryReason::External);
    assert_eq!(included.omitted.filtered, 0);
    assert!(included.partial); // Returned external and unbound calls remain boundaries.
}

#[test]
fn syntax_only_publication_keeps_unclassified_external_looking_calls() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    publisher.publish(&Spec::syntax_only(&[(
        "a.rs",
        "fn root() { std::mem::drop(1); outside(); }
",
    )]));
    let root_id = syntax_id(publisher.native(), "root");
    let result = run(&publisher, &query(&root_id, false, false, false));
    assert_eq!(result.edges.len(), 2);
    assert!(result.edges.iter().all(|edge| edge.binding.is_none()));
    assert_eq!(result.omitted.filtered, 0);
    assert!(result.hints.is_empty());
}

#[test]
fn stored_test_callee_role_filters_before_expansion_without_reading_live_source() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[
        (
            "a.rs",
            "fn root() { unit(); }
",
        ),
        (
            "b.rs",
            "#[test]
fn unit() { child(); }
fn child() {}
",
        ),
    ]);
    spec.bindings = vec![bind("root", 0, "unit"), bind("unit", 0, "child")];
    publisher.publish(&spec);
    let root_id = syntax_id(publisher.native(), "root");
    let test_id = syntax_id(publisher.native(), "unit");
    std::fs::remove_file(root.path().join("b.rs")).unwrap();

    let filtered = run(&publisher, &query(&root_id, true, false, false));
    assert_eq!(filtered.nodes.len(), 1);
    assert!(filtered.edges.is_empty());
    assert_eq!(filtered.omitted.filtered, 1);
    assert_eq!(filtered.hints, vec!["includeTests"]);

    let included = run(&publisher, &query(&root_id, true, false, true));
    assert_eq!(included.nodes.len(), 3);
    assert_eq!(included.edges.len(), 2);
    assert_eq!(included.edges[0].visit, Visit::New);
    let test_node = included
        .nodes
        .iter()
        .find(|node| node.declaration.syntax_id == test_id)
        .unwrap();
    assert_eq!(test_node.declaration.role, DeclarationRole::Test);
    assert_eq!(included.omitted.filtered, 0);
}

#[test]
fn skipped_target_roles_do_not_load_target_document_facts() {
    for (target, max_nodes, reason) in [
        ("#[test]\nfn target() {}\n", 150, FrontierReason::Filtered),
        ("fn target() {}\n", 1, FrontierReason::NodeLimit),
    ] {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut publisher = Publisher::open(state.path(), root.path());
        let mut spec = Spec::new(&[("a.rs", "fn root() { target(); }\n"), ("b.rs", target)]);
        spec.bindings = vec![bind("root", 0, "target")];
        publisher.publish(&spec);
        let root_id = syntax_id(publisher.native(), "root");
        let mut query = query(&root_id, true, false, false);
        query.max_nodes = max_nodes;
        let answer = publisher
            .store
            .evidence_response()
            .unwrap()
            .graph_query(&query, None, &mut || Ok(()))
            .unwrap();
        let result = answer.outcome.as_ref().unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.frontier[0].reason, reason);
        assert_eq!(answer.facts.documents.len(), 1);
        assert_eq!(answer.facts.documents.keys().next().unwrap().path, "a.rs");
        // Byte projection needs only evaluated facts, not the skipped target document.
        let prefix = answer.prefix(0).unwrap();
        assert_eq!(prefix.nodes.len(), 1);
        assert_eq!(
            prefix.hints,
            if reason == FrontierReason::Filtered {
                vec!["includeTests"]
            } else {
                // The node-limit edge is byte-pruned, so its frontier and hint disappear.
                vec!["rootSyntaxId"]
            }
        );
    }
}

#[test]
fn explicit_test_root_keeps_production_calls_and_filters_test_callees() {
    let state = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let mut publisher = Publisher::open(state.path(), root.path());
    let mut spec = Spec::new(&[(
        "a.rs",
        "#[test]
fn root() { production(); outside(); unit(); }
fn production() {}
#[test]
fn unit() {}
",
    )]);
    spec.bindings = vec![
        bind("root", 0, "production"),
        Bind {
            resolution: Resolution::External,
            target: None,
            ..bind("root", 1, "unused")
        },
        bind("root", 2, "unit"),
    ];
    publisher.publish(&spec);
    let root_id = syntax_id(publisher.native(), "root");
    for (external, tests, edges, omitted) in [
        (false, false, 1, 2),
        (true, false, 2, 1),
        (false, true, 2, 1),
        (true, true, 3, 0),
    ] {
        let result = run(&publisher, &query(&root_id, true, external, tests));
        assert_eq!(result.nodes[0].declaration.syntax_id, root_id);
        assert_eq!(result.nodes[0].declaration.role, DeclarationRole::Test);
        assert_eq!(
            result.edges.len(),
            edges,
            "flags external={external} tests={tests}"
        );
        assert_eq!(result.omitted.filtered, omitted);
        assert!(result.partial); // Either filtering or the returned external boundary.
        assert!(!result.truncated);
        assert_eq!(
            result
                .frontier
                .iter()
                .map(|frontier| frontier.omitted_calls)
                .sum::<u64>(),
            omitted
        );
    }
}
