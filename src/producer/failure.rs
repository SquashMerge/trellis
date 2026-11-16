//! Why a run failed, in a few bounded lines (#967, owner ruling 2026-11-12): the exit, the peak
//! resident set, the last stderr lines and, where recognizable, a typed reason. It is derived
//! from the run's diagnostics and stored with the record. It reads data only and reaches no
//! producer module that executes.

use super::Diagnostics;
use serde::{Deserialize, Serialize};

/// The reason for a Node producer that V8 aborted at its heap limit.
pub const NODE_HEAP_EXHAUSTED: &str = "node_heap_exhausted";
/// At most this many stderr lines are kept.
pub const MAX_LINES: usize = 10;
/// Each kept line is cut to at most this many bytes, on a character boundary.
pub const MAX_LINE_BYTES: usize = 512;

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

/// Producers that `node` runs; the test stub stands in for one.
fn node_launched(producer: &str) -> bool {
    matches!(
        producer,
        "scip-python" | "scip-typescript" | crate::scip_admission::STUB_PRODUCER
    )
}

/// Whether `producer` is a Node producer that V8 aborted at its heap limit: it ended with
/// SIGABRT (or code 134) and its stderr shows V8's heap-limit message.
pub fn node_heap_exhausted(producer: &str, diagnostics: &Diagnostics) -> bool {
    let aborted =
        diagnostics.exit.signal == Some(libc::SIGABRT) || diagnostics.exit.code == Some(134);
    let tail = &diagnostics.stderr_tail;
    node_launched(producer)
        && aborted
        && (tail.contains("Reached heap limit") || tail.contains("JavaScript heap out of memory"))
}

fn exit_text(diagnostics: &Diagnostics) -> String {
    let exit = &diagnostics.exit;
    match (exit.code, &exit.signal_name, exit.signal) {
        (Some(code), _, _) => format!("code {code}"),
        (None, Some(name), _) => format!("signal {name}"),
        (None, None, Some(signal)) => format!("signal {signal}"),
        (None, None, None) => "unknown".to_owned(),
    }
}

fn cut(line: &str) -> String {
    let mut end = line.len().min(MAX_LINE_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].to_owned()
}

/// The summary of a run that ended `failed` or `cancelled` with diagnostics; `None` otherwise.
pub fn summarize(
    producer: &str,
    failed: bool,
    diagnostics: Option<&Diagnostics>,
) -> Option<Failure> {
    let diagnostics = diagnostics.filter(|_| failed)?;
    let lines: Vec<&str> = diagnostics
        .stderr_tail
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let kept = &lines[lines.len().saturating_sub(MAX_LINES)..];
    Some(Failure {
        reason: node_heap_exhausted(producer, diagnostics).then(|| NODE_HEAP_EXHAUSTED.to_owned()),
        exit: exit_text(diagnostics),
        peak_rss_bytes: diagnostics.peak_rss_bytes,
        stderr_lines: kept.iter().map(|line| cut(line)).collect(),
    })
}

/// The one stderr line `producer run` prints for a failed run.
pub fn cli_line(code: &str, failure: &Failure) -> String {
    let mib = failure.peak_rss_bytes / (1024 * 1024);
    let hint = if failure.reason.as_deref() == Some(NODE_HEAP_EXHAUSTED) {
        "; raise --memory-mib"
    } else {
        ""
    };
    format!(
        "producer run failed: {code} ({}, peak RSS {mib} MiB){hint}",
        failure.exit
    )
}
