//! Test timing policy. Functional tests never assert speed: a wall-clock bound
//! in a test only detects a hang, and every test uses the same generous
//! backstop. Performance is measured and gated separately. A test that needs
//! progress waits for it; a test that needs ordering or non-blocking behavior
//! proves it with hooks, not elapsed time.
//!
//! # Settled, or the snapshot a reply rendered from (#720)
//!
//! Publication is not synchronous with a request. After a request returns, the
//! watcher and semantic catch-up (S) can still advance the pin or flip the
//! advisory `catchingUp`, and a restarted daemon may still be in startup
//! reconciliation. Two separately sampled readings of a checkout can differ.
//!
//! - **Wait for settled** when a test needs the checkout at rest: before it
//!   compares two separate reads, before it reads after a daemon restart, or
//!   before it asserts that some work left nothing behind. Settled means startup
//!   reconciliation has committed, and the watcher, FIFO and S are idle at one
//!   published head. In process, call
//!   `CheckoutRuntime::await_settled_for_tests` (`settled_for_tests` from async
//!   code). Against a subprocess daemon, run
//!   the hidden `trellis test-settled --workspace <root> [--at <revision>]`
//!   (`settled` in `tests/support/child.rs`). Both wait on the runtime's
//!   published state version, never sleep, and are bounded only by
//!   [`HANG_BACKSTOP`]. A timeout names the component that was still busy.
//! - **Compare against the rendered snapshot** when a test checks one reply, or
//!   one page, for consistency: take the revision the reply itself reports and
//!   assert against that, not against a later status read (#697, #698). This
//!   needs no wait, and stays correct while the checkout keeps moving.
//!
//! Settled describes the moment it was sampled: work a test starts after it
//! returns, or a late watcher event, can make the checkout busy again.
//!
//! # Flaky-pattern check
//!
//! `tools/flaky-patterns.py`, in the verify checks lane, flags these in test
//! code and names the helper to use: an unwrapped `Store::open` (use
//! [`retry_admission`]), a process-global static counter (scope it to the
//! thread, store or call), a functional deadline shorter than
//! [`HANG_BACKSTOP`] (use [`wait_until`], settled, or the backstop), a
//! blocking `accept()` (use [`accept_within_backstop`]), an assert that bounds
//! elapsed time by anything but the backstop, and a socket read timeout below
//! it (prove absence with a later reply instead). Justified exceptions
//! go in `tools/flaky-patterns.allow`, each with a one-line reason.

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

/// Run `open` (such as `|| Store::open(roots.clone(), identity.clone())`) until it
/// succeeds. Only typed admission contention, `storage_busy` from an owner the
/// test does not order (its own daemon, watcher, FIFO or S), is retried, as real
/// CLI clients retry it (#712); any other error panics at once. A test that
/// unwraps the open instead fails whenever that owner holds admission.
pub fn retry_admission<T>(mut open: impl FnMut() -> anyhow::Result<T>) -> T {
    let mut result = None;
    wait_until("index admission", || match open() {
        Ok(value) => {
            result = Some(value);
            true
        }
        Err(error) if crate::store::cli_read_retryable_contention(&error) => false,
        Err(error) => panic!("index admission failed: {error:#}"),
    });
    result.unwrap()
}

/// Accept one connection on a blocking test listener, such as a hook socket,
/// panicking at [`HANG_BACKSTOP`] if none arrives instead of blocking until the
/// test runner kills the test (#742). Reads on the accepted stream are bounded
/// by the same backstop.
pub fn accept_within_backstop(
    listener: &std::os::unix::net::UnixListener,
) -> std::os::unix::net::UnixStream {
    use std::os::fd::AsRawFd;
    let mut ready = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let deadline = Instant::now() + HANG_BACKSTOP;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(
            !left.is_zero(),
            "no connection reached the test listener (hang)"
        );
        let polled = unsafe { libc::poll(&mut ready, 1, left.as_millis() as libc::c_int) };
        if polled == 1 {
            break;
        }
        let error = std::io::Error::last_os_error();
        assert!(
            polled == 0 || error.kind() == std::io::ErrorKind::Interrupted,
            "poll on the test listener failed: {error}"
        );
    }
    let (stream, _) = listener.accept().unwrap();
    stream.set_read_timeout(Some(HANG_BACKSTOP)).unwrap();
    stream
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

/// Set to `on` to give a test-support daemon's checkout watchers real OS
/// delivery. Without it they take synthetic ingress only: a late OS event for
/// a fixture's own setup write would otherwise be honest catch-up at an
/// unchanged head (#703, #720). Only a test of real watcher delivery sets it.
pub const NATIVE_WATCHER_ENV: &str = "TRELLIS_TEST_NATIVE_WATCHER";

/// `command`, whose daemon (or the daemon it starts) watches with real OS
/// delivery. For in-process fixtures, see
/// `CheckoutRegistry::enable_native_watcher_for_tests`.
pub fn with_native_watcher(mut command: std::process::Command) -> std::process::Command {
    command.env(NATIVE_WATCHER_ENV, "on");
    command
}

/// Whether this test-support process was asked for real OS watcher delivery.
#[cfg(feature = "test-support")]
pub(crate) fn native_watcher_requested() -> bool {
    std::env::var_os(NATIVE_WATCHER_ENV).is_some_and(|value| value == "on")
}

/// A fixture process recorded before its PID can become stale. The start time
/// distinguishes an exited fixture from a later process assigned the same PID.
#[derive(Clone, Copy, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProcessIdentity {
    pid: libc::pid_t,
    start_time: u64,
}

impl ProcessIdentity {
    pub fn capture(pid: libc::pid_t) -> Option<Self> {
        (pid > 1).then_some(())?;
        let start_time = crate::producer::process_info::start_time(pid)?;
        Some(Self { pid, start_time })
    }

    pub fn matches(&self) -> bool {
        self.pid > 1 && crate::producer::process_info::start_time(self.pid) == Some(self.start_time)
    }

    /// Recheck immediately before signaling. This is an identity check, not an
    /// atomic OS process handle; direct children should retain their Child owner.
    pub fn signal(&self, signal: libc::c_int) -> std::io::Result<bool> {
        if !self.matches() {
            return Ok(false);
        }
        if unsafe { libc::kill(self.pid, signal) } == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

fn identity_path(marker: &std::path::Path) -> std::path::PathBuf {
    marker.with_extension("identity.json")
}

/// Called by the elected test daemon itself, before it announces readiness.
pub fn write_daemon_marker(marker: &std::path::Path) -> std::io::Result<()> {
    let identity = ProcessIdentity::capture(std::process::id() as libc::pid_t)
        .ok_or_else(|| std::io::Error::other("cannot capture fixture daemon identity"))?;
    std::fs::write(identity_path(marker), serde_json::to_vec(&identity)?)?;
    std::fs::write(marker, identity.pid.to_string())
}

/// Read the elected daemon's self-recorded identity, agreeing with its PID marker.
pub fn daemon_marker_identity(marker: &std::path::Path) -> Option<ProcessIdentity> {
    let pid = std::fs::read_to_string(marker)
        .ok()?
        .parse::<libc::pid_t>()
        .ok()?;
    let identity: ProcessIdentity =
        serde_json::from_slice(&std::fs::read(identity_path(marker)).ok()?).ok()?;
    (identity.pid == pid).then_some(identity)
}

/// Signal only the process whose self-recorded identity agrees with this marker.
/// Missing, malformed and stale markers are not permission to signal a PID.
pub fn signal_daemon_marker(
    marker: &std::path::Path,
    signal: libc::c_int,
) -> std::io::Result<bool> {
    match daemon_marker_identity(marker) {
        Some(identity) => identity.signal(signal),
        None => Ok(false),
    }
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

    struct Sleeper(std::process::Child);
    impl Sleeper {
        fn start() -> Self {
            Self(Command::new("sleep").arg("120").spawn().unwrap())
        }
    }
    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn recorded_identity_rejects_stale_and_special_pids() {
        let mut child = Sleeper::start();
        let identity = super::ProcessIdentity::capture(child.0.id() as libc::pid_t).unwrap();
        let stale = super::ProcessIdentity {
            start_time: identity.start_time + 1,
            ..identity
        };
        assert!(!stale.signal(libc::SIGTERM).unwrap());
        assert!(child.0.try_wait().unwrap().is_none());
        for pid in [0, -1, 1] {
            let invalid = super::ProcessIdentity { pid, ..identity };
            assert!(!invalid.signal(libc::SIGTERM).unwrap());
        }
        assert!(identity.signal(libc::SIGTERM).unwrap());
        child.0.wait().unwrap();
        assert!(!identity.signal(libc::SIGTERM).unwrap());
    }

    #[test]
    fn daemon_marker_requires_matching_self_recorded_identity() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("daemon.pid");
        let mut original = Sleeper::start();
        let mut unrelated = Sleeper::start();
        let identity = super::ProcessIdentity::capture(original.0.id() as libc::pid_t).unwrap();
        std::fs::write(
            super::identity_path(&marker),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        for value in [
            unrelated.0.id().to_string(),
            "0".into(),
            "-1".into(),
            "invalid".into(),
        ] {
            std::fs::write(&marker, value).unwrap();
            assert!(!super::signal_daemon_marker(&marker, libc::SIGTERM).unwrap());
        }
        assert!(unrelated.0.try_wait().unwrap().is_none());
        std::fs::write(&marker, original.0.id().to_string()).unwrap();
        assert!(super::signal_daemon_marker(&marker, libc::SIGTERM).unwrap());
        original.0.wait().unwrap();
        assert!(!super::signal_daemon_marker(&marker, libc::SIGTERM).unwrap());
        std::fs::write(&marker, unrelated.0.id().to_string()).unwrap();
        assert!(!super::signal_daemon_marker(&marker, libc::SIGKILL).unwrap());
        std::fs::remove_file(super::identity_path(&marker)).unwrap();
        assert!(!super::signal_daemon_marker(&marker, libc::SIGTERM).unwrap());
        assert!(unrelated.0.try_wait().unwrap().is_none());
        std::fs::write(super::identity_path(&marker), b"not json").unwrap();
        assert!(!super::signal_daemon_marker(&marker, libc::SIGTERM).unwrap());
        let identity = super::ProcessIdentity::capture(unrelated.0.id() as libc::pid_t).unwrap();
        std::fs::write(
            super::identity_path(&marker),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        assert!(super::signal_daemon_marker(&marker, libc::SIGKILL).unwrap());
        unrelated.0.wait().unwrap();
        assert!(!super::signal_daemon_marker(&marker, libc::SIGKILL).unwrap());
    }

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

    #[test]
    fn accept_within_backstop_takes_a_pending_connection_with_bounded_reads() {
        use std::io::{Read, Write};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hook.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut peer = std::os::unix::net::UnixStream::connect(&path).unwrap();
        peer.write_all(b"x").unwrap();
        let mut accepted = super::accept_within_backstop(&listener);
        assert_eq!(accepted.read_timeout().unwrap(), Some(super::HANG_BACKSTOP));
        let mut byte = [0];
        accepted.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
    }
}
