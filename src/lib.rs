pub mod auth;
pub mod behavior;
mod behavior_bindings;
mod behavior_java;
pub mod behavior_js;
mod behavior_python;
mod behavior_rust;
pub mod capture;
pub mod class_diagram;
pub mod classes;
pub mod daemon;
pub mod dependencies;
pub mod dependency_links;
mod dependency_rust;
pub mod dependency_rust_symbols;
#[cfg(test)]
mod extractor_inputs_tests;
pub mod file_tree;
pub mod files_view;
pub mod git_status;
pub mod http;
pub mod index_coordinator;
pub mod indexer;
mod indexer_java;
#[cfg(test)]
mod indexer_projection_vectors_tests;
mod indexer_python;
mod indexer_rust;
pub mod mcp;
pub mod metrics;
pub mod model;
#[cfg(feature = "test-support")]
pub mod native_capture;
pub mod native_evidence;
pub mod native_ids;
pub mod navigation;
mod parallel;
pub mod references;
pub mod rules;
mod rust_sources;
pub mod scip_admission;
mod scip_raw;
pub mod semantic_evidence;
pub mod semantic_import;
pub mod store;
#[doc(hidden)]
pub mod testing;
pub mod watch;
