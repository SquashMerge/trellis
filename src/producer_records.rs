//! The shared vocabulary of the persisted producer records (S8, #1071): the error codes of
//! producer state, and the process identity, diagnostics and failure summary a run record
//! (`run.json`) and a run reply carry. Storage reads and writes them in
//! `store::producer_state`; the producer commands build them.
use serde::{Deserialize, Serialize};

pub const PRODUCER_STATE_CORRUPT: &str = "producer_state_corrupt";
/// Refusal for producer state that is symlinked, foreign or not private.
pub const UNSAFE_STATE: &str = "invalid_request: unsafe producer state";

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
    pub group_status: GroupStatus,
}

/// What Trellis established about the producer's process group when it stopped
/// owning it (#624). `empty` and `survived` are proven while the group ID was
/// still held; anything that cannot be proven is `unknown`, never assumed empty.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum GroupStatus {
    /// Every member was signalled and only the unreaped leader was left, or, once
    /// the leader was reaped, no process had the group ID at all.
    Empty,
    /// A member was still in the group after the kill wait.
    Survived,
    /// Termination was not established: the final signal was refused or the
    /// group could not be listed, and after the reap a process still had the
    /// group ID (possibly a reuse); or ownership was never provable (crash
    /// recovery).
    #[default]
    Unknown,
}

/// The `failure` of a run reply, `run.json` and `lastRun`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Failure {
    pub reason: Option<String>,
    /// `code <n>` or `signal <NAME>`.
    pub exit: String,
    pub peak_rss_bytes: u64,
    pub stderr_lines: Vec<String>,
}
