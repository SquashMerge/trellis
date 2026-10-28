//! Test timing policy. Functional tests never assert speed: a wall-clock bound
//! in a test only detects a hang, and every test uses the same generous
//! backstop. Performance is measured and gated separately. A test that needs
//! progress waits for it; a test that needs ordering or non-blocking behavior
//! proves it with hooks, not elapsed time.

use std::time::{Duration, Instant};

/// The only wall-clock bound a functional test may use for progress. Well
/// above any healthy run on a loaded host, below nextest's 5-minute kill.
pub const HANG_BACKSTOP: Duration = Duration::from_secs(120);

/// Poll `ready` until it holds; panic with `what` only if it never does.
pub fn wait_until(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + HANG_BACKSTOP;
    while !ready() {
        assert!(Instant::now() < deadline, "{what} did not happen (hang)");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Names the test process that owns a daemon a test-support CLI starts (#130).
pub const OWNER_PID_ENV: &str = "TRELLIS_TEST_OWNER_PID";

/// A `trellis` command owned by this test process. Any daemon it starts exits
/// once this process has ended, however it ends (#130). The owner is set here,
/// at launch, so a test that dies before the CLI runs cannot hand ownership to
/// whatever process adopts the CLI.
///
/// Ignore developer search configuration and index diagnostics. Apply any
/// intentional test-specific environment overrides after calling this helper.
pub fn owned(mut command: std::process::Command) -> std::process::Command {
    command
        .env_remove("TRELLIS_SEARCH_MODEL")
        .env_remove("TRELLIS_SEARCH_BACKEND")
        .env_remove("TRELLIS_INDEX_DIAGNOSTICS")
        .env(OWNER_PID_ENV, std::process::id().to_string());
    command
}

/// Whether `pid` is still running. A process that has exited but not yet been
/// reaped by its parent (a zombie) is not running.
pub fn process_running(pid: libc::pid_t) -> bool {
    if unsafe { libc::kill(pid, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    {
        return false;
    }
    !zombie(pid)
}

#[cfg(target_os = "linux")]
fn zombie(pid: libc::pid_t) -> bool {
    // The state follows the parenthesized command name, which may itself
    // contain ") ".
    std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| {
        stat.rsplit_once(") ")
            .is_none_or(|(_, rest)| matches!(rest.chars().next(), Some('Z' | 'X')))
    })
}

#[cfg(target_os = "macos")]
fn zombie(pid: libc::pid_t) -> bool {
    // SZOMB from <sys/proc.h>.
    const SZOMB: u32 = 5;
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let read =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    // A process that vanished between the two checks is not running either.
    read != size || info.pbi_status == SZOMB
}

#[cfg(test)]
mod tests {
    use super::owned;
    use std::{ffi::OsStr, process::Command};

    #[test]
    fn owned_clears_user_configuration_but_preserves_fixture_environment() {
        let mut command = Command::new("trellis");
        for key in [
            "TRELLIS_SEARCH_MODEL",
            "TRELLIS_SEARCH_BACKEND",
            "TRELLIS_INDEX_DIAGNOSTICS",
        ] {
            command.env(key, "developer-value");
        }
        command
            .env("HOME", "/fixture/home")
            .env("TRELLIS_TEST_EXTRACTOR_HASH", "fixture-hash");
        let command = owned(command);
        for key in [
            "TRELLIS_SEARCH_MODEL",
            "TRELLIS_SEARCH_BACKEND",
            "TRELLIS_INDEX_DIAGNOSTICS",
        ] {
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == key && value.is_none())
            );
        }
        for (key, expected) in [
            ("HOME", "/fixture/home"),
            ("TRELLIS_TEST_EXTRACTOR_HASH", "fixture-hash"),
        ] {
            assert!(
                command
                    .get_envs()
                    .any(|(name, value)| name == key && value == Some(OsStr::new(expected)))
            );
        }
    }

    #[test]
    fn owned_allows_explicit_test_configuration() {
        let mut command = owned(Command::new("trellis"));
        for (key, value) in [
            ("TRELLIS_SEARCH_MODEL", "/fixture/model"),
            ("TRELLIS_SEARCH_BACKEND", "cpu"),
            ("TRELLIS_INDEX_DIAGNOSTICS", "1"),
        ] {
            command.env(key, value);
            assert!(
                command
                    .get_envs()
                    .any(|(name, configured)| name == key && configured == Some(OsStr::new(value)))
            );
        }
    }
}
