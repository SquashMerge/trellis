//! Integration tests: Browser HTTP API and browser selection.
//!
//! Each former `tests/<name>.rs` crate is a module here, so one binary links
//! the library once. nextest still runs every test in its own process.

#[path = "../common/mod.rs"]
mod common;
// Only test-support modules use these helpers, which need its hooks.
#[cfg(feature = "test-support")]
#[path = "../common/graph_fixtures.rs"]
mod graph_fixtures;

mod architecture_http;
mod architecture_precompute;
mod browse_http;
mod browser_route_admission;
mod browser_selection;
mod class_diagram_http;
mod dependency_http;
mod file_tree_http;
mod files_view_http;
mod http;
mod java_python_http;
mod metrics_http;
mod navigation_http;
mod pins_http;
mod rules_edit_http;
mod rust_browse_http;
mod rust_sources_http;
mod topology_http;
