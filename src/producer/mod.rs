//! Explicit, owner-requested semantic producer runs (#21). Nothing in the
//! watcher, the MCP server or daemon launch starts a producer; this module is
//! reached only from the producer commands.

use serde::{Deserialize, Serialize};

pub mod batch;
pub mod detect;
pub mod exec;
pub mod import;
pub mod pickup;
pub mod plan;
pub mod process_info;
pub mod profiles;
pub mod state;
pub mod worktree;

#[cfg(test)]
mod batch_tests;
#[cfg(test)]
mod detect_tests;
#[cfg(test)]
mod pickup_tests;
#[cfg(test)]
mod plan_tests;
#[cfg(test)]
mod process_info_tests;
#[cfg(test)]
mod profiles_tests;
#[cfg(test)]
mod state_tests;
#[cfg(test)]
mod worktree_tests;

pub const PRODUCER_UNAVAILABLE: &str = "producer_unavailable";
pub const PROFILE_UNSUPPORTED: &str = "profile_unsupported";
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
pub const PRODUCER_STATE_CORRUPT: &str = "producer_state_corrupt";
/// Refusal for producer state that is symlinked, foreign or not private.
pub const UNSAFE_STATE: &str = "invalid_request: unsafe producer state";

/// The four producers pinned in the reference hardware profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ProducerName {
    #[serde(rename = "scip-java")]
    ScipJava,
    #[serde(rename = "scip-typescript")]
    ScipTypescript,
    #[serde(rename = "scip-python")]
    ScipPython,
    #[serde(rename = "rust-analyzer")]
    RustAnalyzer,
}

impl ProducerName {
    pub const ALL: [Self; 4] = [
        Self::ScipJava,
        Self::ScipTypescript,
        Self::ScipPython,
        Self::RustAnalyzer,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScipJava => "scip-java",
            Self::ScipTypescript => "scip-typescript",
            Self::ScipPython => "scip-python",
            Self::RustAnalyzer => "rust-analyzer",
        }
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.as_str() == name)
    }
}

/// `restricted` (the default) runs no repository code; `trusted` allows it.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TrustProfile {
    Restricted,
    Trusted,
}

/// A producer's process group, recorded with the leader's start time so a
/// later sweep never signals a reused process ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pgid: i32,
    pub leader_start_time: u64,
}

/// How the producer's leader process ended.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExitReport {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub signal_name: Option<String>,
}

/// Crash and bound diagnostics of one producer process group (AC5e).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Diagnostics {
    pub exit: ExitReport,
    pub duration_ms: u64,
    pub peak_rss_bytes: u64,
    /// The memory bound is sampled, never an address-space limit.
    pub memory_enforcement: String,
    pub artifact_bytes: u64,
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub truncated: bool,
    pub group_survived: bool,
}
