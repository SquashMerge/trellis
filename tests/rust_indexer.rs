mod common;
use protobuf::Message;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    indexer::{IndexOptions, index_workspace},
    model::*,
};
fn run(o: &IndexOptions) -> Graph {
    index_workspace(o, &Arc::new(AtomicBool::new(false)), |_| {}).unwrap()
}
fn hash(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}
#[test]
fn mixed_discovery_exact_ranges_owners_macros_and_utf8() {
    let d = tempfile::tempdir().unwrap();
    let source = r#"mod child { pub fn café() { outer(inner()); } }
struct Thing;
trait Work { fn signature(&self); fn defaulted(&self) { fallback(); } }
impl Work for Thing { fn signature(&self) { invoke((|x| closure_call(x))); self.next(); } }
macro_rules! generated { () => { fn invisible() { hidden(); } } }
#[cfg(feature = "unknown")]
fn configured() { opaque!(not_a_call()); let c = || separate(); after(); }
"#;
    fs::write(d.path().join("lib.rs"), source).unwrap();
    fs::write(d.path().join("script.js"), "function js() { jsCall(); }").unwrap();
    fs::create_dir(d.path().join("target")).unwrap();
    fs::write(d.path().join("target/ignored.rs"), "fn ignored() {}").unwrap();
    let o = IndexOptions::new(d.path().to_owned());
    let g = run(&o);
    assert_eq!(g, run(&o));
    assert_eq!(g.stats.files, 2);
    assert_eq!(g.stats.parse_error_files, 0);
    assert!(
        g.diagnostics
            .iter()
            .any(|d| d.path.as_deref() == Some("lib.rs")
                && d.code == "native-coverage-partial"
                && d.message.contains("opaque Rust"))
    );
    assert_eq!(
        g.files
            .iter()
            .find(|f| f.path == "lib.rs")
            .unwrap()
            .language,
        "rust"
    );
    let cafe = g.nodes.iter().find(|n| n.name == "café").unwrap();
    assert_eq!(
        &source[cafe.range.start_byte..cafe.range.end_byte],
        "pub fn café() { outer(inner()); }"
    );
    assert_eq!(cafe.kind, SymbolKind::Function);
    let calls: Vec<_> = g.calls.iter().filter(|c| c.caller == cafe.id).collect();
    assert_eq!(
        calls
            .iter()
            .map(|c| c.callee_text.as_deref())
            .collect::<Vec<_>>(),
        [Some("outer"), Some("inner")]
    );
    assert_eq!(calls.iter().map(|c| c.ordinal).collect::<Vec<_>>(), [0, 1]);
    // #22 retains source-measured function signatures, without inferring a call edge.
    assert!(
        g.nodes
            .iter()
            .any(|n| n.name == "signature" && n.kind == SymbolKind::Method)
    );
    assert!(
        !g.calls
            .iter()
            .any(|c| c.callee_text.as_deref() == Some("signature"))
    );
    let closure_call = g
        .calls
        .iter()
        .find(|c| c.callee_text.as_deref() == Some("closure_call"))
        .unwrap();
    let closure = g
        .nodes
        .iter()
        .find(|n| n.id == closure_call.caller)
        .unwrap();
    assert_eq!(
        &source[closure.range.start_byte..closure.range.end_byte],
        "|x| closure_call(x)"
    );
    let invoke = g
        .calls
        .iter()
        .find(|c| c.callee_text.as_deref() == Some("invoke"))
        .unwrap();
    assert_ne!(invoke.caller, closure.id);
    assert_ne!(invoke.caller, closure_call.caller);
    assert!(!g.nodes.iter().any(|n| n.name == "invisible"));
    assert!(!g.calls.iter().any(|c| matches!(
        c.callee_text.as_deref(),
        Some("hidden" | "not_a_call" | "opaque")
    )));
    let separate = g
        .calls
        .iter()
        .find(|c| c.callee_text.as_deref() == Some("separate"))
        .unwrap();
    let after = g
        .calls
        .iter()
        .find(|c| c.callee_text.as_deref() == Some("after"))
        .unwrap();
    assert_ne!(separate.caller, after.caller);
    assert!(g.calls.iter().filter(|c| c.path == "lib.rs").all(
        |c| c.id.starts_with("occ:v2:") && c.provenance.semantic == SemanticState::Unavailable
    ));
    assert!(g.nodes.iter().all(|n| n.id.starts_with("sid:v1:")));
}
#[test]
fn recovered_rust_retains_syntax_and_cached_source() {
    let d = tempfile::tempdir().unwrap();
    let source = "fn healthy() { café(); }\nfn broken( {";
    fs::write(d.path().join("bad.rs"), source).unwrap();
    let g = run(&IndexOptions::new(d.path().to_owned()));
    assert_eq!(g.stats.parse_error_files, 1);
    assert!(g.nodes.iter().any(|n| n.name == "healthy"));
    fs::remove_file(d.path().join("bad.rs")).unwrap();
    assert_eq!(g.files[0].text, source);
    assert!(
        g.calls
            .iter()
            .all(|c| c.provenance.semantic == SemanticState::Unavailable)
    );
}
#[test]
fn cargo_hash_inputs_and_fresh_scip_do_not_resolve_rust() {
    let d = tempfile::tempdir().unwrap();
    let source = "fn run() { external(); }";
    let manifest = "[package]\nname = 'untrusted'\nversion = '0.0.0'\nbuild = 'build.rs'\n";
    let lock = "# lexical hash input only\n";
    let build = "fn main() { panic!(\"must never execute\"); }";
    let inputs = [
        ("lib.rs", source),
        ("Cargo.toml", manifest),
        ("Cargo.lock", lock),
        ("build.rs", build),
    ];
    let mut hashes = BTreeMap::new();
    for (path, text) in inputs {
        fs::write(d.path().join(path), text).unwrap();
        hashes.insert(path, hash(text));
    }
    let mut index = scip::types::Index::new();
    let mut doc = scip::types::Document::new();
    doc.relative_path = "lib.rs".into();
    let mut occurrence = scip::types::Occurrence::new();
    occurrence.range = vec![0, 11, 19];
    occurrence.symbol = "scip fake package 1 external().".into();
    doc.occurrences.push(occurrence);
    index.documents.push(doc);
    fs::write(d.path().join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
    fs::write(
        d.path().join("manifest.json"),
        serde_json::to_vec(&hashes).unwrap(),
    )
    .unwrap();
    let o = IndexOptions::new(d.path().to_owned());
    let g = run(&o);
    assert_eq!(g.stats.semantic_state, SemanticState::Unavailable);
    assert!(
        g.calls
            .iter()
            .all(|c| c.provenance.semantic == SemanticState::Unavailable)
    );
    assert!(
        g.nodes
            .iter()
            .all(|n| n.provenance.semantic == SemanticState::Unavailable)
    );
    fs::write(d.path().join("Cargo.lock"), "changed").unwrap();
    fs::write(d.path().join("Cargo.toml"), "changed").unwrap();
    let stale = run(&o);
    assert_eq!(stale.stats.semantic_state, SemanticState::Unavailable);
    assert_eq!(stale.calls.len(), g.calls.len());
    // Cargo inputs change the revision, not the native extraction context (Decision 0003),
    // so unchanged documents keep their occ:v2 IDs.
    assert_eq!(stale.calls[0].id, g.calls[0].id);
}

#[test]
fn chained_calls_have_unique_range_ids_and_publish() {
    let d = tempfile::tempdir().unwrap();
    let source = "fn run() { receiver().write(inner(first()), second()); factory().method(other()); std::fs::write(path(), data()).unwrap(); }";
    fs::write(d.path().join("lib.rs"), source).unwrap();
    let options = IndexOptions::new(d.path().to_owned());
    let graph = run(&options);
    let ids: std::collections::BTreeSet<_> = graph.calls.iter().map(|c| &c.id).collect();
    assert_eq!(ids.len(), graph.calls.len());
    assert_eq!(graph, run(&options));
    assert!(graph.calls.iter().any(|a| {
        graph.calls.iter().any(|b| {
            a.range.start_byte == b.range.start_byte && a.range.end_byte != b.range.end_byte
        })
    }));
    let state = tempfile::tempdir().unwrap();
    let store = crate::common::open_store(&state.path().join("state"), d.path()).unwrap();
    let (_pin, _session) = trellis::index_coordinator::reconcile_workspace(
        &store,
        &options,
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    let published = store.graph().unwrap();
    assert_eq!(published.files, graph.files);
    assert_eq!(published.nodes, graph.nodes);
    let mut actual_calls = published.calls;
    let mut measured_calls = graph.calls;
    actual_calls.sort_by(|a, b| a.id.cmp(&b.id));
    measured_calls.sort_by(|a, b| a.id.cmp(&b.id));
    assert_eq!(
        actual_calls, measured_calls,
        "published calls must match measured Rust source"
    );
}
