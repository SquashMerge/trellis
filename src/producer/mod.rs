//! Explicit, owner-requested semantic producer runs (#21). Nothing in the
//! watcher, the MCP server or daemon launch starts a producer; this module is
//! reached only from the producer commands and, for a workspace that opted in
//! and a manual run approved, from the checkout's `AutoRunOwner` (`auto.rs`,
//! #660). `install` runs a pinned install command only from the explicit
//! `producer install` CLI command (#880).

pub use crate::producer_profiles::{PROFILE_UNSUPPORTED, ProducerName, TrustProfile};
pub use crate::producer_records::{
    Diagnostics, ExitReport, GroupStatus, PRODUCER_STATE_CORRUPT, ProcessIdentity, UNSAFE_STATE,
};

pub mod approval;
pub mod auto;
pub mod auto_config;
pub mod auto_gate;
pub mod batch;
pub mod detect;
pub mod doctor;
pub mod exec;
pub mod failure;
pub mod import;
pub mod install;
pub mod pickup;
pub mod pickup_watch;
pub mod plan;
pub use crate::process_info;
pub mod profiles;
pub(crate) mod retention;
pub mod run;
pub mod settings;
pub mod state;
pub mod tsconfig;
pub mod worktree;

#[cfg(test)]
mod approval_tests;
#[cfg(test)]
mod auto_config_tests;
#[cfg(test)]
mod auto_gate_tests;
#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod detect_tests;
#[cfg(test)]
mod exec_tests;
#[cfg(test)]
mod pickup_tests;
#[cfg(test)]
mod pickup_watch_tests;
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod process_info_tests;
#[cfg(test)]
mod profiles_tests;
#[cfg(test)]
mod run_tests;
#[cfg(test)]
mod settings_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod tsconfig_tests;
#[cfg(test)]
mod worktree_tests;

pub const PRODUCER_UNAVAILABLE: &str = "producer_unavailable";
pub const PLAN_CHANGED: &str = "plan_changed";
pub const PRODUCER_BUSY: &str = "producer_busy";
pub const NOT_CANCELLABLE: &str = "not_cancellable";
pub const NOT_FOUND: &str = "not_found";
pub const INDEX_NOT_READY: &str = "index_not_ready";
pub const CAPTURE_UNAVAILABLE: &str = "capture_unavailable";
pub const INVALID_REQUEST: &str = "invalid_request";
pub const ROOT_CHANGED: &str = "root_changed";
/// A producer artifact that cannot be turned into a batch.
pub const PRODUCER_OUTPUT_INVALID: &str = "producer_output_invalid";
