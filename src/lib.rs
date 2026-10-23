pub mod acp;
pub mod answer;
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
pub mod native_evidence;
pub mod native_ids;
pub mod navigation;
mod parallel;
mod rust_sources;
pub mod semantic_evidence;
pub mod store;
#[doc(hidden)]
pub mod testing;
pub mod watch;

pub mod jev;
pub mod live_jev;
pub mod planning;
