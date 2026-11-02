//! Integration tests: Native indexing, evidence, references, schema recreation and atomic publication.
//!
//! Each former `tests/<name>.rs` crate is a module here, so one binary links
//! the library once. nextest still runs every test in its own process.

#[path = "../common/mod.rs"]
mod common;

mod anchors;
mod identity;
mod index_atomic;
mod indexer;
mod java_indexer;
mod native_capture;
mod native_evidence;
mod native_id_vectors;
mod native_search_capture;
mod native_store;
mod native_whitespace;
mod proptest_absence;
mod python_indexer;
mod references;
mod rust_indexer;
mod schema11_recreate;
mod schema12_recreate;
mod schema13_recreate;
mod schema14_recreate;
mod sqlite_witness;
