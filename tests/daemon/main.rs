//! Integration tests: Daemon, CLI, request queue, watcher and checkout lifecycle.
//!
//! Each former `tests/<name>.rs` crate is a module here, so one binary links
//! the library once. nextest still runs every test in its own process.

#[path = "../common/mod.rs"]
mod common;

mod build_identity;
mod cli;
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
