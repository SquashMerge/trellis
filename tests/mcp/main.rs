//! Integration tests: MCP protocol and graph-query surfaces.
//!
//! Each former `tests/<name>.rs` crate is a module here, so one binary links
//! the library once. nextest still runs every test in its own process.

// Only test-support modules use these helpers, which need its hooks.
#[cfg(feature = "test-support")]
#[path = "../common/graph_fixtures.rs"]
mod graph_fixtures;
#[cfg(feature = "test-support")]
#[path = "../support/semantic_batch.rs"]
mod semantic_batch_support;

mod graph_query;
mod graph_query_store;
mod graph_query_store_filters;
mod mcp_protocol;
mod navigation_resolved;
mod sequence_columns;
mod sequence_incidental;
