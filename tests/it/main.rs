//! All integration tests build as one binary (#243). Linking the library and
//! its dependencies once, instead of once per test file, is most of the test
//! build. nextest still runs every test in its own process. Each file keeps
//! its place under tests/ and becomes a module named after it.

#[path = "../acp_http.rs"]
mod acp_http;
#[path = "../anchors.rs"]
mod anchors;
#[path = "../answer.rs"]
mod answer;
#[path = "../behavior.rs"]
mod behavior;
#[path = "../browse_http.rs"]
mod browse_http;
#[path = "../browser_selection.rs"]
mod browser_selection;
#[path = "../class_diagram_http.rs"]
mod class_diagram_http;
#[path = "../classes.rs"]
mod classes;
#[path = "../cli.rs"]
mod cli;
#[path = "../common/mod.rs"]
mod common;
#[path = "../daemon_checkout.rs"]
mod daemon_checkout;
#[path = "../daemon_cli.rs"]
mod daemon_cli;
#[path = "../daemon_crash_recovery.rs"]
mod daemon_crash_recovery;
#[path = "../daemon_http.rs"]
mod daemon_http;
#[path = "../daemon_lifecycle.rs"]
mod daemon_lifecycle;
#[path = "../daemon_mcp.rs"]
mod daemon_mcp;
#[path = "../daemon_registry.rs"]
mod daemon_registry;
#[path = "../daemon_socket.rs"]
mod daemon_socket;
#[path = "../dependencies.rs"]
mod dependencies;
#[path = "../dependency_declarations.rs"]
mod dependency_declarations;
#[path = "../dependency_http.rs"]
mod dependency_http;
#[path = "../dependency_links.rs"]
mod dependency_links;
#[path = "../file_tree.rs"]
mod file_tree;
#[path = "../file_tree_http.rs"]
mod file_tree_http;
#[path = "../gc_record_report.rs"]
mod gc_record_report;
#[path = "../http.rs"]
mod http;
#[path = "../identity.rs"]
mod identity;
#[path = "../index_atomic.rs"]
mod index_atomic;
#[path = "../index_coordinator.rs"]
mod index_coordinator;
#[path = "../indexer.rs"]
mod indexer;
#[path = "../java_behavior.rs"]
mod java_behavior;
#[path = "../java_indexer.rs"]
mod java_indexer;
#[path = "../java_python_http.rs"]
mod java_python_http;
#[path = "../jev.rs"]
mod jev;
#[path = "../common/jev_wire.rs"]
mod jev_wire;
#[path = "../live_jev.rs"]
mod live_jev;
#[path = "../live_jev_http.rs"]
mod live_jev_http;
#[path = "../mcp_protocol.rs"]
mod mcp_protocol;
#[path = "../metrics.rs"]
mod metrics;
#[path = "../metrics_http.rs"]
mod metrics_http;
#[path = "../metrics_oracle.rs"]
mod metrics_oracle;
#[path = "../metrics_store.rs"]
mod metrics_store;
#[path = "../native_evidence.rs"]
mod native_evidence;
#[path = "../native_id_vectors.rs"]
mod native_id_vectors;
#[path = "../native_store.rs"]
mod native_store;
#[path = "../navigation_http.rs"]
mod navigation_http;
#[path = "../pins.rs"]
mod pins;
#[path = "../pins_http.rs"]
mod pins_http;
#[path = "../planning.rs"]
mod planning;
#[path = "../pre_startup_read.rs"]
mod pre_startup_read;
#[path = "../privacy.rs"]
mod privacy;
#[path = "../python_behavior.rs"]
mod python_behavior;
#[path = "../python_indexer.rs"]
mod python_indexer;
#[path = "../questions_http.rs"]
mod questions_http;
#[path = "../requests.rs"]
mod requests;
#[path = "../rust_behavior.rs"]
mod rust_behavior;
#[path = "../rust_browse_http.rs"]
mod rust_browse_http;
#[path = "../rust_indexer.rs"]
mod rust_indexer;
#[path = "../rust_sources_http.rs"]
mod rust_sources_http;
#[path = "../sequence_pele_fixtures.rs"]
mod sequence_pele_fixtures;
#[path = "../sqlite_witness.rs"]
mod sqlite_witness;
#[path = "../store.rs"]
mod store;
#[path = "../topology.rs"]
mod topology;
#[path = "../topology_http.rs"]
mod topology_http;
#[path = "../watch.rs"]
mod watch;

/// Cargo no longer discovers test files (`autotests = false`), so a new file
/// under tests/ must be listed above or it would silently never build.
#[test]
fn every_test_file_is_a_module_of_this_binary() {
    let tests = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let listed = std::fs::read_to_string(tests.join("it/main.rs")).unwrap();
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(&tests).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name.ends_with(".rs") && !listed.contains(&format!("#[path = \"../{name}\"]")) {
            missing.push(name);
        }
    }
    assert!(missing.is_empty(), "add to tests/it/main.rs: {missing:?}");
}
