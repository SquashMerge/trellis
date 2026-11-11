//! Integration tests: Daemon, CLI, request queue, watcher and checkout lifecycle.
//!
//! Each former `tests/<name>.rs` crate is a module here, so one binary links
//! the library once. nextest still runs every test in its own process.

#[path = "../support/child.rs"]
mod child;
#[path = "../common/mod.rs"]
mod common;
// Only the in-process automatic-run tests use these, and they need the test-support hooks.
#[cfg(feature = "test-support")]
#[path = "../support/semantic_batch.rs"]
mod semantic_batch_support;
#[cfg(feature = "test-support")]
#[path = "../support/producer.rs"]
mod support;

mod build_identity;
mod cli;
mod cli_activation;
mod daemon_auto_run;
mod daemon_checkout;
mod daemon_cli;
mod daemon_crash_recovery;
mod daemon_http;
mod daemon_lifecycle;
mod daemon_mcp;
mod daemon_registry;
mod daemon_socket;
mod gc_record_report;
mod git_status;
mod index_coordinator;
mod pre_startup_read;
mod requests;
mod watch;
