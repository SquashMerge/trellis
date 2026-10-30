#[cfg(feature = "test-support")]
use std::collections::BTreeSet;
use std::{
    fs,
    sync::{Arc, atomic::AtomicBool},
};
#[cfg(feature = "test-support")]
use trellis::indexer::index_workspace_bundle;
use trellis::{
    indexer::{IndexOptions, index_workspace},
    model::*,
};
fn run(o: &IndexOptions) -> Graph {
    index_workspace(o, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap()
}
#[test]
fn lexical_owners_constructors_lambdas_anonymous_classes_and_utf8() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"package demo;
@interface Mark { String value() default "not invocation"; }
interface Work { void signature(); default void fallback() { backup(); } }
record Pair(String name) { Pair { validate(name); } }
enum Mode { ON { void act() { enumOnly(); } }; }
@Mark("héllo")
class Café {
    Café() { this(argument()); }
    Café(Object value) { super(); }
    native void nativeOnly();
    void run() {
        use((x) -> lambdaOnly(x));
        Runnable r = () -> { nestedOnly(); };
        Object a = new Base(constructorArg()) {
            { initializerOnly(); }
            void inside() { anonymousOnly(); }
        };
        class Local { void local() { localOnly(); } }
        if (check()) { receiver().next(inner()); }
        after();
    }
}
"#;
    fs::write(dir.path().join("Café.java"), source).unwrap();
    fs::write(dir.path().join("script.js"), "function js() { jsOnly(); }").unwrap();
    fs::write(dir.path().join("lib.rs"), "fn rust() { rust_only(); }").unwrap();
    let opts = IndexOptions::new(dir.path().to_owned());
    let g = run(&opts);
    assert_eq!(g, run(&opts));
    assert_eq!(g.stats.parse_error_files, 0, "{:?}", g.diagnostics);
    assert_eq!(g.files.len(), 3);
    let class = g
        .nodes
        .iter()
        .find(|n| n.name == "Café" && n.kind == SymbolKind::Class)
        .unwrap();
    let method = g.nodes.iter().find(|n| n.name == "run").unwrap();
    assert_eq!(method.parent.as_ref(), Some(&class.id));
    let constructors: Vec<_> = g
        .nodes
        .iter()
        .filter(|n| n.name == "Café" && n.kind == SymbolKind::Method)
        .collect();
    assert_eq!(constructors.len(), 2);
    assert!(
        constructors
            .iter()
            .all(|c| c.parent.as_ref() == Some(&class.id))
    );
    let call = |name: &str| {
        g.calls
            .iter()
            .find(|c| c.callee_text.as_deref() == Some(name))
            .unwrap()
    };
    assert_eq!(call("after").caller, method.id);
    assert_eq!(call("constructorArg").caller, method.id);
    let lambda = g
        .nodes
        .iter()
        .find(|n| n.id == call("lambdaOnly").caller)
        .unwrap();
    assert_eq!(lambda.parent.as_ref(), Some(&method.id));
    assert_ne!(call("use").caller, lambda.id);
    assert!(g.nodes.iter().all(|n| n.id.starts_with("sid:v1:")));
    for c in g.calls.iter().filter(|c| c.path.ends_with(".java")) {
        assert!(c.id.starts_with("occ:v2:"));
        assert!(g.nodes.iter().any(|n| n.id == c.caller));
        assert_eq!(c.provenance.semantic, SemanticState::Unavailable);
        assert!(
            source.is_char_boundary(c.range.start_byte)
                && source.is_char_boundary(c.range.end_byte)
        );
        assert_eq!(
            c.range.start_line,
            source[..c.range.start_byte]
                .bytes()
                .filter(|b| *b == b'\n')
                .count()
                + 1
        );
        let line_start = source[..c.range.start_byte]
            .rfind('\n')
            .map_or(0, |i| i + 1);
        assert_eq!(c.range.start_column, c.range.start_byte - line_start + 1);
        if let Some(r) = &c.callee_range {
            assert_eq!(
                &source[r.start_byte..r.end_byte],
                c.callee_text.as_deref().unwrap()
            );
        }
    }
    let graph_json = serde_json::to_string(&g).unwrap();
    assert!(!graph_json.contains("callbackArguments") && !graph_json.contains("candidateSymbols"));
}
#[cfg(feature = "test-support")]
#[test]
fn shared_start_call_ids_are_unique_and_publishable() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Calls.java"),
        "class Calls { void run() { receiver().next(inner(first()), second()).finish(); } }",
    )
    .unwrap();
    let graph = run(&IndexOptions::new(dir.path().to_owned()));
    assert_eq!(
        graph
            .calls
            .iter()
            .map(|c| &c.id)
            .collect::<BTreeSet<_>>()
            .len(),
        graph.calls.len()
    );
    assert!(graph.calls.iter().any(|a| {
        graph.calls.iter().any(|b| {
            a.range.start_byte == b.range.start_byte && a.range.end_byte != b.range.end_byte
        })
    }));
    let state = tempfile::tempdir().unwrap();
    let store = crate::common::open_store(&state.path().join("state"), dir.path()).unwrap();
    let (bundle_graph, native, capture) = index_workspace_bundle(
        &IndexOptions::new(dir.path().to_owned()),
        store.root_id(),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(graph, bundle_graph);
    store
        .publish_native(
            &bundle_graph,
            &capture,
            &native,
            &store.publication_for_tests().unwrap(),
            store.index_baseline().unwrap(),
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
}
#[test]
fn recovery_and_cancellation_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("Broken.java"),
        "class Broken { void run() { call( ; } }",
    )
    .unwrap();
    let options = IndexOptions::new(dir.path().to_owned());
    let graph = run(&options);
    assert_eq!(graph.stats.parse_error_files, 1);
    assert!(graph.diagnostics.iter().any(|d| d.code == "parse-error"));
    assert!(
        index_workspace(&options, &Arc::new(AtomicBool::new(true)), |_| {})
            .unwrap_err()
            .to_string()
            .contains("cancel")
    );
}
