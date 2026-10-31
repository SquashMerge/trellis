//! Integration tests: producer orchestration: plans, runs, pickup, triggers, GC and real-producer end-to-end.
//!
//! Each former `tests/<name>.rs` crate is a module here, so one binary links
//! the library once. nextest still runs every test in its own process.

#[path = "../common/mod.rs"]
mod common;
// Only test-support modules use these helpers, which need its hooks.
#[cfg(feature = "test-support")]
#[path = "../support/real_producers.rs"]
mod real;
#[cfg(feature = "test-support")]
#[path = "../support/semantic_batch.rs"]
mod semantic_batch_support;
#[cfg(feature = "test-support")]
#[path = "../support/producer.rs"]
mod support;

mod producer_e2e;
mod producer_e2e_java;
mod producer_e2e_rust;
mod producer_exec;
mod producer_gc;
mod producer_pickup;
mod producer_pickup_import;
mod producer_plan;
mod producer_profiles;
mod producer_run;
mod producer_spawn_inventory;
mod producer_triggers;
