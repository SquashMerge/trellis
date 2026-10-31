//! Integration tests: Store, views, metrics, rules, search and language behavior.
//!
//! Each former `tests/<name>.rs` crate is a module here, so one binary links
//! the library once. nextest still runs every test in its own process.

#[path = "../common/mod.rs"]
mod common;
// Only test-support modules use these helpers, which need its hooks.
#[cfg(feature = "test-support")]
#[path = "../common/graph_fixtures.rs"]
mod graph_fixtures;

mod architecture_external;
mod architecture_semantic;
mod architecture_store;
mod architecture_view;
mod behavior;
mod classes;
mod dependencies;
mod dependency_declarations;
mod dependency_links;
mod file_tree;
mod files_view_store;
mod java_behavior;
mod metrics;
mod metrics_oracle;
mod metrics_store;
mod pins;
mod privacy;
mod python_behavior;
mod rules;
mod rust_behavior;
mod search_card;
mod search_embed;
mod search_lexical;
mod search_model;
mod search_service;
mod search_store;
mod search_vectors;
mod sequence_pele_fixtures;
mod store;
mod synthetic_cohorts;
mod topology;
