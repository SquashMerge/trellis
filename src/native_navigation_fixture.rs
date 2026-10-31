//! Same-image validated publication for the real browser acceptance suite.
#[path = "../tests/common/graph_fixtures.rs"]
mod graph_fixtures;

use anyhow::Result;
use graph_fixtures::{Publisher, Spec, add_second_producer, bind, calls_of, syntax_id};
use serde_json::{Value, json};
use std::path::Path;
use trellis::{
    semantic_evidence::{Dispatch, Resolution},
    store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    },
};

pub fn publish(root: &Path, mode: &str) -> Result<Value> {
    trellis::capture::pin_running_executable()?;
    let identity = WorkspaceIdentity::discover(Some(root), root)?;
    let store = Store::open(TopologyRoots::production()?, identity)?;
    let mut publisher = Publisher::from_store(store, root);
    let sequence = mode.starts_with("sequence-");
    let mode = mode.strip_prefix("sequence-").unwrap_or(mode);
    let caller = if sequence {
        "fn measured() {\n    lexical_decoy();\n    lexical_decoy();\n}\nfn lexical_decoy() {}\n"
    } else {
        "fn measured() {\n    lexical_decoy();\n}\nfn lexical_decoy() {}\n"
    };
    let files = [
        ("flow.rs", caller),
        (
            "target.rs",
            if mode == "constructor" {
                "pub struct actual_target;\n"
            } else {
                "pub fn actual_target() {}\n"
            },
        ),
    ];
    let mut spec = if mode == "zero" {
        Spec::syntax_only(&files)
    } else {
        Spec::new(&files)
    };
    if mode == "stale" {
        spec.requested_version = "v2".into();
    }
    // An external call into a package the browser's library catalog can open (#450).
    if mode == "library" {
        spec.external_symbol = "rust-analyzer cargo local-library 0.1.0 candidate_only().".into();
    }
    for nth in 0..if sequence { 2 } else { 1 } {
        let mut binding = bind("measured", nth, "actual_target");
        if sequence {
            // Groupable graph calls still need separate sequence occurrence evidence.
            binding.provenance = "shared-calls".into();
        }
        match mode {
            "unresolved" => binding.resolution = Resolution::Unresolved,
            "ambiguous" => binding.resolution = Resolution::Ambiguous,
            "dispatch" => binding.dispatch = Dispatch::Virtual,
            "constructor" => binding.dispatch = Dispatch::Constructor,
            "external" | "library" => binding.resolution = Resolution::External,
            "missing" => continue,
            _ => {}
        }
        spec.bindings.push(binding);
    }
    let pin = if mode == "multiple" {
        publisher.publish_with_semantic(&spec, add_second_producer)
    } else {
        publisher.publish(&spec)
    };
    Ok(json!({
        "revision": pin,
        "caller": syntax_id(publisher.native(), "measured"),
        "callee": syntax_id(publisher.native(), "actual_target"),
        "decoy": syntax_id(publisher.native(), "lexical_decoy"),
        "calls": calls_of(publisher.native(), "measured")
            .iter().map(|call| &call.id).collect::<Vec<_>>()
    }))
}
