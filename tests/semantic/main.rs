//! Integration tests: Semantic import, batch store, publication and rejoin.
//!
//! Each former `tests/<name>.rs` crate is a module here, so one binary links
//! the library once. nextest still runs every test in its own process.

// Only test-support modules use these helpers, which need its hooks.
#[cfg(feature = "test-support")]
#[path = "../support/semantic_batch.rs"]
mod semantic_batch_support;
#[cfg(feature = "test-support")]
#[path = "../support/producer.rs"]
mod support;

mod java_semantic_import;
mod semantic_batch_queue;
mod semantic_batch_store;
mod semantic_evidence;
mod semantic_generated_import;
mod semantic_history;
mod semantic_import;
mod semantic_import_queue;
mod semantic_import_typed_errors;
mod semantic_native_publication;
mod semantic_publication_paths;
mod semantic_rejoin;
mod semantic_store;
