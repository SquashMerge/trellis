mod common;
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
fn run(options: &IndexOptions) -> Graph {
    index_workspace(options, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap()
}
#[cfg(feature = "test-support")]
#[test]
fn scopes_defaults_decorators_async_lambdas_utf8_and_publication() {
    let dir = tempfile::tempdir().unwrap();
    let source = "@decorate(factory())\nclass Café(base()):\n    class_value = initialize()\n    @method_decorator()\n    async def run(self, arg=default()):\n        def nested(x=nested_default()):\n            nested_body()\n        use(lambda x=lambda_default(): lambda_body(x))\n        receiver().write(inner(first()), second())\n        await request()\n        if check():\n            return finish()\n";
    fs::write(dir.path().join("sample.py"), source).unwrap();
    fs::write(dir.path().join("other.js"), "function js() { invoke(); }").unwrap();
    let options = IndexOptions::new(dir.path().to_owned());
    let graph = run(&options);
    assert_eq!(graph, run(&options));
    assert_eq!(graph.stats.parse_error_files, 0);
    assert_eq!(graph.stats.files, 2);
    let owner = |callee: &str| {
        graph
            .nodes
            .iter()
            .find(|n| {
                n.id == graph
                    .calls
                    .iter()
                    .find(|c| c.callee_text.as_deref() == Some(callee))
                    .unwrap()
                    .caller
            })
            .unwrap()
    };
    assert_eq!(owner("nested_body").name, "nested");
    assert_eq!(owner("nested_body").kind, SymbolKind::Function);
    assert_eq!(owner("request").name, "run");
    assert_ne!(owner("lambda_body").id, owner("use").id);
    let class = graph.nodes.iter().find(|n| n.name == "Café").unwrap();
    assert!(source[class.range.start_byte..class.range.end_byte].starts_with("class Café"));
    let ids: BTreeSet<_> = graph.calls.iter().map(|c| &c.id).collect();
    assert_eq!(ids.len(), graph.calls.len());
    assert!(graph.calls.iter().any(|a| {
        graph.calls.iter().any(|b| {
            a.range.start_byte == b.range.start_byte && a.range.end_byte != b.range.end_byte
        })
    }));
    for call in graph.calls.iter().filter(|c| c.path == "sample.py") {
        assert_eq!(call.provenance.semantic, SemanticState::Unavailable);
        assert!(call.id.starts_with("occ:v2:"));
        assert!(graph.nodes.iter().any(|n| n.id == call.caller));
        assert!(
            source.is_char_boundary(call.range.start_byte)
                && source.is_char_boundary(call.range.end_byte)
        );
        let prefix = &source[..call.range.start_byte];
        assert_eq!(
            call.range.start_line,
            prefix.bytes().filter(|b| *b == b'\n').count() + 1
        );
        assert_eq!(
            call.range.start_column,
            prefix.rsplit('\n').next().unwrap().len() + 1
        );
    }
    assert!(!graph.regions.is_empty());
    let state = tempfile::tempdir().unwrap();
    let store = crate::common::open_store(&state.path().join("state"), dir.path()).unwrap();
    let cancel = Arc::new(AtomicBool::new(false));
    let (bundle_graph, native, capture) =
        index_workspace_bundle(&options, store.root_id(), &cancel, |_| {}).unwrap();
    assert_eq!(graph, bundle_graph);
    let baseline = store.index_baseline().unwrap();
    let pin = store
        .publish_native(
            &bundle_graph,
            &capture,
            &native,
            &store.publication_for_tests().unwrap(),
            baseline,
            &cancel,
        )
        .unwrap();
    assert_eq!(pin.index_revision, 1);
    assert!(
        store
            .publish_native(
                &bundle_graph,
                &capture,
                &native,
                &store.publication_for_tests().unwrap(),
                pin,
                &Arc::new(AtomicBool::new(true))
            )
            .is_err()
    );
}
#[test]
fn recovery_annotations_and_cancellation_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let source =
        "def healthy(x: annotation() = default()) -> returned():\n    café()\ndef broken(\n";
    fs::write(dir.path().join("bad.py"), source).unwrap();
    let options = IndexOptions::new(dir.path().to_owned());
    let graph = run(&options);
    assert_eq!(graph.stats.parse_error_files, 1);
    assert!(graph.nodes.iter().any(|n| n.name == "healthy"));
    // Annotation syntax is source-measured but never execution or semantic proof.
    for name in ["annotation", "returned"] {
        for call in graph
            .calls
            .iter()
            .filter(|c| c.callee_text.as_deref() == Some(name))
        {
            let r = call.callee_range.as_ref().unwrap();
            assert_eq!(&source[r.start_byte..r.end_byte], name);
            assert_eq!(call.provenance.semantic, SemanticState::Unavailable);
        }
    }
    assert!(
        graph
            .calls
            .iter()
            .any(|c| c.callee_text.as_deref() == Some("café"))
    );
    assert!(index_workspace(&options, &Arc::new(AtomicBool::new(true)), |_| {}).is_err());
    fs::remove_file(dir.path().join("bad.py")).unwrap();
    assert_eq!(graph.files[0].text, source);
    assert!(graph.diagnostics.iter().any(|d| d.code == "parse-error"));
}
