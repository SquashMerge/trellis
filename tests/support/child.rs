//! The standard failure message for a child process a test runs, usually a Trellis CLI (#720).
//!
//! A failure shows the child's exit status, its stderr, its stdout (for the CLI, the JSON reply
//! with `state`, `errorCode` and `diagnostics`) and, when the fixture kept one through
//! `TRELLIS_TEST_DAEMON_STDERR`, the tail of the test daemon's stderr. Nothing here formats the
//! `Command` or its environment, so a token or key passed to the child is never printed.
//!
//! Include it at the test crate root as `#[path = "../support/child.rs"] mod child;`.
#![allow(dead_code)]
use serde_json::Value;
use std::{
    fmt::{Display, Write as _},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

/// The test-support hook that sends an auto-started daemon's stderr to a file.
pub const DAEMON_STDERR: &str = "TRELLIS_TEST_DAEMON_STDERR";
/// Each of the child's streams is shown whole up to this size, else its head and tail.
const STREAM_BYTES: usize = 64 * 1024;
/// The daemon stderr is shown from its end, up to this size.
const DAEMON_TAIL_BYTES: usize = 16 * 1024;

/// The daemon stderr file `command` names through `TRELLIS_TEST_DAEMON_STDERR`, if it sets one.
pub fn daemon_stderr_of(command: &Command) -> Option<PathBuf> {
    command
        .get_envs()
        .find(|(key, _)| *key == DAEMON_STDERR)
        .and_then(|(_, value)| value.map(PathBuf::from))
}

/// The standard message for `output` of the child described by `context`.
pub fn failure(context: impl Display, output: &Output, daemon_stderr: Option<&Path>) -> String {
    let mut message = format!("{context}: child failed ({})", output.status);
    section(&mut message, "stderr", &output.stderr);
    section(&mut message, "stdout", &output.stdout);
    if let Some(path) = daemon_stderr {
        match fs::read(path) {
            Ok(text) => {
                let tail = &text[text.len().saturating_sub(DAEMON_TAIL_BYTES)..];
                let _ = write!(
                    message,
                    "\n--- daemon stderr tail ({}, last {} of {} bytes) ---\n{}",
                    path.display(),
                    tail.len(),
                    text.len(),
                    String::from_utf8_lossy(tail)
                );
            }
            Err(error) => {
                let _ = write!(
                    message,
                    "\n--- daemon stderr ({}) unavailable: {error} ---",
                    path.display()
                );
            }
        }
    }
    message
}

fn section(message: &mut String, name: &str, bytes: &[u8]) {
    let _ = write!(message, "\n--- {name} ({} bytes) ---\n", bytes.len());
    if bytes.len() <= STREAM_BYTES {
        message.push_str(&String::from_utf8_lossy(bytes));
    } else {
        let half = STREAM_BYTES / 2;
        let _ = write!(
            message,
            "{}\n[... {} bytes elided ...]\n{}",
            String::from_utf8_lossy(&bytes[..half]),
            bytes.len() - 2 * half,
            String::from_utf8_lossy(&bytes[bytes.len() - half..])
        );
    }
}

/// Panics with the standard message unless the child succeeded.
#[track_caller]
pub fn assert_cli_success(output: &Output, context: impl Display) {
    assert_cli_success_with(output, context, None);
}

/// [`assert_cli_success`], adding the tail of the daemon stderr file the fixture kept.
#[track_caller]
pub fn assert_cli_success_with(output: &Output, context: impl Display, daemon: Option<&Path>) {
    if !output.status.success() {
        panic!("{}", failure(context, output, daemon));
    }
}

/// The child's JSON reply on stdout, after [`assert_cli_success`].
#[track_caller]
pub fn cli_json(output: &Output, context: impl Display) -> Value {
    cli_json_with(output, context, None)
}

/// [`cli_json`], adding the tail of the daemon stderr file the fixture kept.
#[track_caller]
pub fn cli_json_with(output: &Output, context: impl Display, daemon: Option<&Path>) -> Value {
    assert_cli_success_with(output, &context, daemon);
    match serde_json::from_slice(&output.stdout) {
        Ok(reply) => reply,
        Err(error) => panic!(
            "{}",
            failure(
                format_args!("{context}: stdout is not JSON ({error})"),
                output,
                daemon
            )
        ),
    }
}

/// Runs `command` to completion and asserts it succeeded. The message includes the daemon
/// stderr tail when `command` sets `TRELLIS_TEST_DAEMON_STDERR`.
#[track_caller]
pub fn run_cli(command: &mut Command, context: impl Display) -> Output {
    let daemon = daemon_stderr_of(command);
    let output = command.output().unwrap();
    assert_cli_success_with(&output, context, daemon.as_deref());
    output
}

/// Waits until the daemon `command` reaches has settled the checkout at `workspace`: startup
/// reconciliation committed, and its watcher, FIFO and semantic catch-up idle (#720). Starts
/// the daemon if none runs. `command` is a test-support `trellis` command with the test's
/// environment and no arguments. With `at`, a revision as `trellis status` prints it, the
/// checkout must settle at that published head. Returns the settled revision.
///
/// The wait is on the daemon's published state, bounded only by the hang backstop; a failure
/// names the component still busy. See `trellis::testing` for when to use it.
#[track_caller]
pub fn settled(mut command: Command, workspace: &Path, at: Option<&Value>) -> Value {
    command
        .arg("test-settled")
        .arg("--workspace")
        .arg(workspace);
    if let Some(at) = at {
        command.arg("--at").arg(at.to_string());
    }
    run_cli_json(&mut command, "settled")["revision"].clone()
}

/// [`run_cli`], returning the child's JSON reply.
#[track_caller]
pub fn run_cli_json(command: &mut Command, context: impl Display) -> Value {
    let daemon = daemon_stderr_of(command);
    let output = command.output().unwrap();
    cli_json_with(&output, context, daemon.as_deref())
}
