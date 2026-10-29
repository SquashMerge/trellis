//! Same-image validated publication for the real browser acceptance suite.
#[path = "../tests/common/graph_fixtures.rs"]
mod graph_fixtures;

use anyhow::Result;
use graph_fixtures::{Publisher, Spec, add_second_producer, bind, syntax_id};
use serde_json::{Value, json};
use std::path::Path;
use trellis::{
    semantic_evidence::Resolution,
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
    let files = [
        (
            "flow.rs",
            "fn measured() {\n    lexical_decoy();\n}\nfn lexical_decoy() {}\n",
        ),
        ("target.rs", "pub fn actual_target() {}\n"),
    ];
    let mut spec = if mode == "zero" {
        Spec::syntax_only(&files)
    } else {
        Spec::new(&files)
    };
    let mut binding = bind("measured", 0, "actual_target");
    if mode == "unresolved" {
        binding.resolution = Resolution::Unresolved;
    }
    spec.bindings.push(binding);
    let pin = if mode == "multiple" {
        publisher.publish_with_semantic(&spec, add_second_producer)
    } else {
        publisher.publish(&spec)
    };
    Ok(json!({
        "revision": pin,
        "caller": syntax_id(publisher.native(), "measured"),
        "callee": syntax_id(publisher.native(), "actual_target"),
        "decoy": syntax_id(publisher.native(), "lexical_decoy")
    }))
}
