//! The only place a producer process is started (D8). The child leads its own
//! session and process group; every bound terminates the whole group, and the
//! run's diagnostics report how it ended.

use super::process_info;
use super::{
    CompleteDiagnostics, ExitReport, ProcessIdentity, TerminationFailure, TerminationStatus,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{self, Read, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt, process::ExitStatusExt},
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// The last bytes of each output stream kept in the diagnostics.
pub const TAIL_BYTES: usize = 16 * 1024;
/// The monitor loop never sleeps longer than this between checks.
const TICK: Duration = Duration::from_millis(100);
/// How long to wait for the output readers once the group is gone.
const READER_WAIT: Duration = Duration::from_secs(2);

/// What to run. `env` is the complete child environment: nothing is inherited.
#[derive(Clone, Debug)]
pub struct ProducerSpec {
    pub argv: Vec<OsString>,
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
    /// The file the producer writes; its size is bounded.
    pub artifact: PathBuf,
    /// Receives `stdout` and `stderr` (capped on disk) when set.
    pub log_dir: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_bytes: u64,
    pub artifact_bytes: u64,
    /// On-disk bytes per output stream.
    pub log_bytes: u64,
    /// Wait between SIGTERM and SIGKILL.
    pub grace: Duration,
    pub sample: Duration,
    /// Backstop for the group to empty after SIGKILL.
    pub kill_wait: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(1800),
            memory_bytes: 8192 * 1024 * 1024,
            artifact_bytes: crate::semantic_batch::limits::BatchLimits::PRODUCTION.artifact_bytes,
            log_bytes: 4 * 1024 * 1024,
            grace: Duration::from_secs(2),
            sample: Duration::from_millis(500),
            kill_wait: Duration::from_secs(10),
        }
    }
}

impl Limits {
    /// `TRELLIS_TEST_PRODUCER_LIMITS={"artifactBytes","logBytes","graceMs","sampleMs"}`.
    #[cfg(feature = "test-support")]
    pub fn with_test_overrides(mut self) -> Self {
        let Some(raw) = std::env::var_os("TRELLIS_TEST_PRODUCER_LIMITS") else {
            return self;
        };
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw.as_encoded_bytes()) else {
            return self;
        };
        let number = |key: &str| value.get(key).and_then(serde_json::Value::as_u64);
        if let Some(n) = number("artifactBytes") {
            self.artifact_bytes = n;
        }
        if let Some(n) = number("logBytes") {
            self.log_bytes = n;
        }
        if let Some(n) = number("graceMs") {
            self.grace = Duration::from_millis(n);
        }
        if let Some(n) = number("sampleMs") {
            self.sample = Duration::from_millis(n);
        }
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitKind {
    Memory,
    ArtifactBytes,
}

impl LimitKind {
    /// The typed reason after `limit_exceeded: `.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Memory => "producer memory",
            Self::ArtifactBytes => "producer artifact bytes",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Ending {
    /// The leader exited on its own.
    Exited,
    TimedOut,
    Cancelled,
    LimitExceeded(LimitKind),
    /// Artifact metadata could not be measured for a reason other than absence.
    ArtifactMetadataUnavailable,
}

#[derive(Debug)]
pub struct ProcessOutcome {
    pub ending: Ending,
    pub process: ProcessIdentity,
    pub diagnostics: CompleteDiagnostics,
}

/// A spawned producer whose termination could not be established.
#[derive(Debug)]
pub struct ProcessFailure {
    pub kind: TerminationFailure,
    pub detail: String,
    /// An independently observed bound or cancellation before termination failed.
    pub ending: Option<Ending>,
}

impl std::fmt::Display for ProcessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "producer_failed: termination unknown: {}", self.detail)
    }
}

impl std::error::Error for ProcessFailure {}

fn process_failure(kind: TerminationFailure, detail: impl std::fmt::Display) -> anyhow::Error {
    ProcessFailure {
        kind,
        detail: detail.to_string(),
        ending: None,
    }
    .into()
}

fn finish_for_ending<P: ProcessOps>(
    owner: &mut ActiveProcess<'_, P>,
    ending: Ending,
) -> Result<(ExitStatus, Option<bool>)> {
    owner.finish().map_err(|mut error| {
        if ending != Ending::Exited
            && let Some(failure) = error.downcast_mut::<ProcessFailure>()
        {
            failure.ending = Some(ending);
        }
        error
    })
}

/// Callbacks at the points a run owner or a test needs to observe.
#[derive(Default)]
pub struct Hooks<'a> {
    /// The child is running and its group identity is known.
    pub spawned: Option<&'a mut dyn FnMut(&ProcessIdentity)>,
}

/// Output of one stream: bytes seen, bytes written to disk, and the tail.
#[derive(Default)]
struct StreamState {
    total: u64,
    written: u64,
    tail: Vec<u8>,
    reached_eof: bool,
}

struct Reader {
    state: Arc<Mutex<StreamState>>,
    thread: JoinHandle<()>,
}

fn start_reader(
    mut source: impl Read + Send + 'static,
    mut file: Option<fs::File>,
    cap: u64,
) -> Reader {
    let state = Arc::new(Mutex::new(StreamState::default()));
    let shared = Arc::clone(&state);
    let thread = std::thread::spawn(move || {
        let mut buffer = [0_u8; 8192];
        loop {
            let read = match source.read(&mut buffer) {
                Ok(0) => {
                    if let Ok(mut state) = shared.lock() {
                        state.reached_eof = true;
                    }
                    return;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
                Ok(read) => read,
            };
            let chunk = &buffer[..read];
            let Ok(mut state) = shared.lock() else { return };
            state.total = state.total.saturating_add(read as u64);
            let room = cap.saturating_sub(state.written) as usize;
            let keep = room.min(read);
            if keep > 0
                && let Some(out) = file.as_mut()
            {
                // A full disk loses log bytes, never the run.
                if out.write_all(&chunk[..keep]).is_ok() {
                    state.written += keep as u64;
                } else {
                    file = None;
                }
            }
            state.tail.extend_from_slice(chunk);
            if state.tail.len() > TAIL_BYTES {
                let excess = state.tail.len() - TAIL_BYTES;
                state.tail.drain(..excess);
            }
        }
    });
    Reader { state, thread }
}

impl Reader {
    /// Wait briefly for end of stream: a descendant that left the group may
    /// hold the pipe open, and that must not hang the run.
    fn finish(self, cap: u64) -> (u64, String, bool, bool) {
        self.finish_with_wait(cap, READER_WAIT)
    }

    fn finish_with_wait(self, cap: u64, wait: Duration) -> (u64, String, bool, bool) {
        let deadline = Instant::now() + wait;
        while !self.thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let state = self.state.lock().map_or_else(
            |poisoned| {
                let state = poisoned.into_inner();
                (state.total, state.tail.clone(), false)
            },
            |state| (state.total, state.tail.clone(), state.reached_eof),
        );
        (
            state.0,
            String::from_utf8_lossy(&state.1).into_owned(),
            state.0 > cap,
            state.2,
        )
    }
}

fn open_log(spec: &ProducerSpec, name: &str) -> Result<Option<fs::File>> {
    let Some(dir) = &spec.log_dir else {
        return Ok(None);
    };
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dir.join(name))
        .map(Some)
        .with_context(|| format!("create {name} log"))
}

fn signal_name(signal: i32) -> String {
    match signal {
        libc::SIGHUP => "SIGHUP".into(),
        libc::SIGINT => "SIGINT".into(),
        libc::SIGQUIT => "SIGQUIT".into(),
        libc::SIGILL => "SIGILL".into(),
        libc::SIGABRT => "SIGABRT".into(),
        libc::SIGFPE => "SIGFPE".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGBUS => "SIGBUS".into(),
        libc::SIGSEGV => "SIGSEGV".into(),
        libc::SIGPIPE => "SIGPIPE".into(),
        libc::SIGALRM => "SIGALRM".into(),
        libc::SIGTERM => "SIGTERM".into(),
        other => format!("SIG{other}"),
    }
}

fn exit_report(status: ExitStatus) -> ExitReport {
    ExitReport {
        code: status.code(),
        signal: status.signal(),
        signal_name: status.signal().map(signal_name),
    }
}

fn artifact_len(spec: &ProducerSpec) -> io::Result<Option<u64>> {
    metadata_or_missing(fs::metadata(&spec.artifact).map(|meta| meta.len()))
}

/// Only absence is a missing artifact; other metadata errors remain errors.
pub(super) fn metadata_or_missing<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

// Process-wide invariant: Trellis does not install a SIGCHLD reaper or perform
// broad waits. Tokio waits only on its own children. Nobody else waits on this
// Child or changes SIGCHLD to an auto-reaping disposition during a run. This is
// ordinary child ownership, not a PID/start-time revalidation scheme.
fn check_sigchld() -> io::Result<()> {
    let mut action = std::mem::MaybeUninit::<libc::sigaction>::zeroed();
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), action.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let action = unsafe { action.assume_init() };
    if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::other("SIGCHLD auto-reaps children"));
    }
    Ok(())
}

fn reap_eventually(
    mut wait: impl FnMut() -> io::Result<ExitStatus>,
    mut sleep: impl FnMut(Duration),
) {
    let mut delay = Duration::from_millis(10);
    loop {
        match wait() {
            Ok(_) => return,
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return,
            Err(_) => {
                sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(1));
            }
        }
    }
}

trait ProcessOps {
    /// Observe without consuming wait ownership.
    fn exited(&mut self) -> io::Result<bool>;
    fn signal(&mut self, signal: i32) -> io::Result<()>;
    fn reap(&mut self) -> io::Result<ExitStatus>;
    fn abandon_wait(&mut self, no_child: bool) -> io::Result<()>;
}

struct OwnedChild {
    child: Option<Child>,
    pgid: i32,
    reaper: std::sync::mpsc::Sender<Child>,
}

impl ProcessOps for OwnedChild {
    fn exited(&mut self) -> io::Result<bool> {
        loop {
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pgid as libc::id_t,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == 0 {
                return Ok(unsafe { info.assume_init().si_pid() } == self.pgid);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn signal(&mut self, signal: i32) -> io::Result<()> {
        if unsafe { libc::killpg(self.pgid, signal) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn reap(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.as_mut().expect("owned child").wait()?;
        self.child.take();
        Ok(status)
    }

    fn abandon_wait(&mut self, no_child: bool) -> io::Result<()> {
        if let Some(child) = self.child.take() {
            if no_child {
                // ECHILD says this is no longer a waitable child. Waiting on
                // the cached PID later could reap a different, reused child.
                return Ok(());
            }
            // Only reaping remains. The prestarted thread has no authority to
            // signal; if it unexpectedly disappeared, never signal on fallback.
            self.reaper
                .send(child)
                .map_err(|_| io::Error::other("eventual reaper unavailable"))?;
        }
        Ok(())
    }
}

/// Sole owner of the child and hence of group-signaling authority. It is armed
/// immediately after spawn, before any fallible setup or user hook.
struct ActiveProcess<'a, P: ProcessOps> {
    ops: P,
    limits: &'a Limits,
    armed: bool,
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod ownership_tests {
    use super::*;
    use std::{cell::RefCell, collections::VecDeque, rc::Rc};

    #[test]
    fn artifact_metadata_distinguishes_absence_from_io_error() {
        let missing = metadata_or_missing::<u64>(Err(io::Error::from(io::ErrorKind::NotFound)));
        assert!(matches!(missing, Ok(None)));
        let denied =
            metadata_or_missing::<u64>(Err(io::Error::from(io::ErrorKind::PermissionDenied)));
        assert_eq!(denied.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }

    fn metadata_test_spec(dir: &std::path::Path, argv: &[&str]) -> ProducerSpec {
        ProducerSpec {
            argv: argv.iter().map(Into::into).collect(),
            cwd: dir.to_owned(),
            env: BTreeMap::new(),
            artifact: dir.join("missing.scip"),
            log_dir: None,
        }
    }

    fn metadata_test_limits() -> Limits {
        Limits {
            timeout: Duration::from_secs(2),
            grace: Duration::from_millis(10),
            kill_wait: Duration::from_secs(2),
            sample: Duration::from_millis(5),
            ..Limits::default()
        }
    }

    #[test]
    fn metadata_error_during_live_run_fails_without_a_known_size() {
        let dir = tempfile::tempdir().unwrap();
        let spec = metadata_test_spec(dir.path(), &["/bin/sleep", "1"]);
        let mut calls = 0;
        let outcome = spawn_producer_with_artifact_size(
            &spec,
            &metadata_test_limits(),
            &AtomicBool::new(false),
            &mut Hooks::default(),
            || {
                calls += 1;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        )
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(outcome.ending, Ending::ArtifactMetadataUnavailable);
        assert_eq!(outcome.diagnostics.artifact_bytes, None);
        assert_eq!(
            serde_json::to_value(&outcome.diagnostics).unwrap()["artifactBytes"],
            serde_json::Value::Null
        );
        assert_eq!(outcome.diagnostics.termination, TerminationStatus::Unknown);
        assert_eq!(outcome.diagnostics.group_survived, None);
    }

    #[test]
    fn final_metadata_error_after_owned_leader_exit_fails_without_size() {
        let dir = tempfile::tempdir().unwrap();
        let spec = metadata_test_spec(dir.path(), &["/bin/sh", "-c", "exit 0"]);
        let mut wait_for_exit = |identity: &ProcessIdentity| {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
                let rc = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        identity.pgid as libc::id_t,
                        info.as_mut_ptr(),
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                assert_eq!(rc, 0);
                if unsafe { info.assume_init().si_pid() } == identity.pgid {
                    break;
                }
                assert!(Instant::now() < deadline, "leader did not exit");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        let mut calls = 0;
        let outcome = spawn_producer_with_artifact_size(
            &spec,
            &metadata_test_limits(),
            &AtomicBool::new(false),
            &mut Hooks {
                spawned: Some(&mut wait_for_exit),
            },
            || {
                calls += 1;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        )
        .unwrap();
        assert_eq!(calls, 1, "only the final measurement is reached");
        assert_eq!(outcome.ending, Ending::ArtifactMetadataUnavailable);
        assert_eq!(outcome.diagnostics.exit.code, Some(0));
        assert_eq!(outcome.diagnostics.artifact_bytes, None);
        assert_eq!(outcome.diagnostics.group_survived, None);
    }

    #[test]
    fn cancellation_is_preserved_when_artifact_metadata_is_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let spec = metadata_test_spec(dir.path(), &["/bin/sleep", "1"]);
        let cancel = AtomicBool::new(false);
        let outcome = spawn_producer_with_artifact_size(
            &spec,
            &metadata_test_limits(),
            &cancel,
            &mut Hooks::default(),
            || {
                cancel.store(true, Ordering::SeqCst);
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        )
        .unwrap();
        assert_eq!(outcome.ending, Ending::Cancelled);
        assert_eq!(outcome.diagnostics.artifact_bytes, None);
        assert_eq!(outcome.diagnostics.termination, TerminationStatus::Unknown);
        assert_eq!(outcome.diagnostics.group_survived, None);
    }

    #[test]
    fn already_requested_cancellation_survives_final_metadata_error() {
        let dir = tempfile::tempdir().unwrap();
        let spec = metadata_test_spec(dir.path(), &["/bin/sleep", "1"]);
        let mut calls = 0;
        let outcome = spawn_producer_with_artifact_size(
            &spec,
            &metadata_test_limits(),
            &AtomicBool::new(true),
            &mut Hooks::default(),
            || {
                calls += 1;
                Err(io::Error::from(io::ErrorKind::PermissionDenied))
            },
        )
        .unwrap();
        assert_eq!(calls, 1, "the only measurement is after cancellation");
        assert_eq!(outcome.ending, Ending::Cancelled);
        assert_eq!(outcome.diagnostics.artifact_bytes, None);
        assert_eq!(outcome.diagnostics.termination, TerminationStatus::Unknown);
        assert_eq!(outcome.diagnostics.group_survived, None);
    }

    #[test]
    fn absent_artifact_remains_missing_not_metadata_failure() {
        let dir = tempfile::tempdir().unwrap();
        let spec = metadata_test_spec(dir.path(), &["/bin/sh", "-c", "exit 0"]);
        assert!(matches!(artifact_len(&spec), Ok(None)));
        let outcome = spawn_producer(
            &spec,
            &metadata_test_limits(),
            &AtomicBool::new(false),
            &mut Hooks::default(),
        )
        .unwrap();
        assert_eq!(outcome.ending, Ending::Exited);
        assert_eq!(outcome.diagnostics.exit.code, Some(0));
        assert_eq!(outcome.diagnostics.artifact_bytes, None);
        assert_eq!(outcome.diagnostics.group_survived, None);
    }

    #[test]
    fn measured_empty_and_oversized_artifacts_keep_their_real_sizes() {
        for (script, expected_size, expected_ending) in [
            (": > \"$1\"", 0, Ending::Exited),
            (
                "printf 1234 > \"$1\"",
                4,
                Ending::LimitExceeded(LimitKind::ArtifactBytes),
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let mut spec = metadata_test_spec(dir.path(), &["/bin/sh", "-c", script, "producer"]);
            spec.argv.push(spec.artifact.clone().into_os_string());
            let limits = Limits {
                artifact_bytes: 2,
                sample: Duration::from_secs(60),
                ..metadata_test_limits()
            };
            let outcome = spawn_producer(
                &spec,
                &limits,
                &AtomicBool::new(false),
                &mut Hooks::default(),
            )
            .unwrap();
            assert_eq!(outcome.ending, expected_ending);
            assert_eq!(outcome.diagnostics.artifact_bytes, Some(expected_size));
        }
    }

    #[test]
    fn reader_error_is_incomplete_even_after_thread_exits() {
        struct Failing(bool);
        impl Read for Failing {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.0 {
                    return Err(io::Error::other("injected read error"));
                }
                self.0 = true;
                out[..3].copy_from_slice(b"abc");
                Ok(3)
            }
        }
        let reader = start_reader(Failing(false), None, 16);
        let (bytes, tail, truncated, complete) = reader.finish_with_wait(16, READER_WAIT);
        assert_eq!((bytes, tail.as_str(), truncated), (3, "abc", false));
        assert!(!complete, "a failed read is not EOF");
    }

    #[test]
    fn reader_open_pipe_returns_partial_count_and_incomplete_status() {
        use std::os::unix::net::UnixStream;

        struct NotifyingPipe {
            source: UnixStream,
            next_read: Option<std::sync::mpsc::Sender<()>>,
            done: std::sync::mpsc::Sender<()>,
            read_once: bool,
        }
        impl Read for NotifyingPipe {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                if self.read_once
                    && let Some(ready) = self.next_read.take()
                {
                    let _ = ready.send(());
                }
                self.read_once = true;
                self.source.read(out)
            }
        }
        impl Drop for NotifyingPipe {
            fn drop(&mut self) {
                let _ = self.done.send(());
            }
        }

        let (mut writer, source) = UnixStream::pair().unwrap();
        writer.write_all(b"abc").unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let reader = start_reader(
            NotifyingPipe {
                source,
                next_read: Some(ready_tx),
                done: done_tx,
                read_once: false,
            },
            None,
            16,
        );
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let (bytes, tail, truncated, complete) = reader.finish_with_wait(16, Duration::ZERO);
        assert_eq!(
            (bytes, tail.as_str(), truncated, complete),
            (3, "abc", false, false)
        );
        drop(writer);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn reader_retries_interruption_and_marks_observed_eof_complete() {
        struct InterruptedThenEof(u8);
        impl Read for InterruptedThenEof {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                self.0 += 1;
                match self.0 {
                    1 => Err(io::Error::from(io::ErrorKind::Interrupted)),
                    2 => {
                        out[..3].copy_from_slice(b"abc");
                        Ok(3)
                    }
                    _ => Ok(0),
                }
            }
        }
        let reader = start_reader(InterruptedThenEof(0), None, 16);
        let (bytes, tail, truncated, complete) = reader.finish_with_wait(16, READER_WAIT);
        assert_eq!(
            (bytes, tail.as_str(), truncated, complete),
            (3, "abc", false, true)
        );
    }

    #[test]
    fn eventual_reaper_retries_transient_wait_errors_until_success() {
        let mut results = VecDeque::from([
            Err(io::Error::from_raw_os_error(libc::EINTR)),
            Err(io::Error::other("temporary")),
            Ok(ExitStatus::from_raw(0)),
        ]);
        let mut calls = 0;
        let mut delays = Vec::new();
        reap_eventually(
            || {
                calls += 1;
                results.pop_front().unwrap()
            },
            |delay| delays.push(delay),
        );
        assert_eq!(calls, 3);
        assert_eq!(
            delays,
            [Duration::from_millis(10), Duration::from_millis(20)]
        );
    }

    #[test]
    fn eventual_reaper_stops_on_echild_after_transient_error() {
        let mut results = VecDeque::from([
            Err(io::Error::other("temporary")),
            Err(io::Error::from_raw_os_error(libc::ECHILD)),
        ]);
        let mut calls = 0;
        let mut delays = Vec::new();
        reap_eventually(
            || {
                calls += 1;
                results.pop_front().unwrap()
            },
            |delay| delays.push(delay),
        );
        assert_eq!(calls, 2);
        assert_eq!(delays, [Duration::from_millis(10)]);
    }

    #[test]
    fn eventual_reaper_backoff_is_capped() {
        let mut calls = 0;
        let mut delays = Vec::new();
        reap_eventually(
            || {
                calls += 1;
                if calls <= 12 {
                    Err(io::Error::other("temporary"))
                } else {
                    Err(io::Error::from_raw_os_error(libc::ECHILD))
                }
            },
            |delay| delays.push(delay),
        );
        assert_eq!(calls, 13);
        assert_eq!(delays[0], Duration::from_millis(10));
        assert_eq!(delays[7], Duration::from_secs(1));
        assert!(delays.iter().all(|delay| *delay <= Duration::from_secs(1)));
    }

    struct Fake {
        events: Rc<RefCell<Vec<&'static str>>>,
        observations: VecDeque<io::Result<bool>>,
        _absent: bool,
        signal_fail: bool,
        signal_errors: VecDeque<Option<i32>>,
        signal_panic: bool,
        reap_fail: bool,
        reap_echild: bool,
    }

    impl ProcessOps for Fake {
        fn exited(&mut self) -> io::Result<bool> {
            self.events.borrow_mut().push("observe");
            self.observations.pop_front().unwrap_or(Ok(true))
        }
        fn signal(&mut self, signal: i32) -> io::Result<()> {
            self.events.borrow_mut().push(if signal == libc::SIGTERM {
                "term"
            } else {
                "kill"
            });
            assert!(!self.signal_panic, "injected signal panic");
            if let Some(Some(error)) = self.signal_errors.pop_front() {
                Err(io::Error::from_raw_os_error(error))
            } else if self.signal_fail {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            } else {
                Ok(())
            }
        }
        fn reap(&mut self) -> io::Result<ExitStatus> {
            self.events.borrow_mut().push("reap");
            if self.reap_echild {
                Err(io::Error::from_raw_os_error(libc::ECHILD))
            } else if self.reap_fail {
                Err(io::Error::other("injected reap failure"))
            } else {
                Ok(ExitStatus::from_raw(0))
            }
        }
        fn abandon_wait(&mut self, no_child: bool) -> io::Result<()> {
            self.events
                .borrow_mut()
                .push(if no_child { "released" } else { "abandon" });
            Ok(())
        }
    }

    fn limits() -> Limits {
        Limits {
            grace: Duration::ZERO,
            kill_wait: Duration::ZERO,
            ..Limits::default()
        }
    }

    fn fake(
        events: &Rc<RefCell<Vec<&'static str>>>,
        observations: Vec<io::Result<bool>>,
        absent: bool,
    ) -> Fake {
        Fake {
            events: events.clone(),
            observations: observations.into(),
            _absent: absent,
            signal_fail: false,
            signal_errors: VecDeque::new(),
            signal_panic: false,
            reap_fail: false,
            reap_echild: false,
        }
    }

    #[test]
    fn exited_leader_stays_pinned_through_both_signals() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut owner = ActiveProcess {
            ops: fake(&events, vec![], true),
            limits: &limits,
            armed: true,
        };
        assert!(owner.observe().unwrap());
        let (status, survived) = owner.finish().unwrap();
        assert!(status.success());
        assert_eq!(survived, None);
        drop(owner);
        assert_eq!(
            *events.borrow(),
            [
                "observe", "observe", "term", "observe", "kill", "observe", "reap"
            ]
        );
    }

    #[test]
    fn forced_termination_preserves_unknown_survival() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut owner = ActiveProcess {
            ops: fake(
                &events,
                vec![Ok(false), Ok(false), Ok(false), Ok(true)],
                false,
            ),
            limits: &limits,
            armed: true,
        };
        assert!(!owner.observe().unwrap());
        assert_eq!(owner.finish().unwrap().1, None);
        assert_eq!(
            *events.borrow(),
            [
                "observe", "observe", "term", "observe", "kill", "observe", "reap"
            ]
        );
    }

    #[test]
    fn unwind_uses_same_owner_once() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _owner = ActiveProcess {
                ops: fake(&events, vec![], false),
                limits: &limits,
                armed: true,
            };
            panic!("injected setup panic");
        }));
        assert_eq!(
            *events.borrow(),
            ["observe", "term", "observe", "kill", "observe", "reap"]
        );
    }

    #[test]
    fn cleanup_panic_is_caught_and_handoffs_without_another_signal() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut ops = fake(&events, vec![Ok(false)], false);
        ops.signal_panic = true;
        let mut owner = ActiveProcess {
            ops,
            limits: &limits,
            armed: true,
        };
        let error = owner.finish().unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProcessFailure>().unwrap().kind,
            TerminationFailure::Unwind
        );
        drop(owner);
        assert_eq!(*events.borrow(), ["observe", "term", "abandon"]);
    }

    #[test]
    fn lost_wait_ownership_never_signals_again() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut owner = ActiveProcess {
            ops: fake(
                &events,
                vec![Ok(false), Err(io::Error::from_raw_os_error(libc::ECHILD))],
                false,
            ),
            limits: &limits,
            armed: true,
        };
        assert!(owner.finish().is_err());
        drop(owner);
        assert_eq!(*events.borrow(), ["observe", "term", "observe", "released"]);
    }

    #[test]
    fn cleanup_failure_retains_the_monitors_observed_ending() {
        for ending in [
            Ending::TimedOut,
            Ending::Cancelled,
            Ending::LimitExceeded(LimitKind::Memory),
            Ending::LimitExceeded(LimitKind::ArtifactBytes),
        ] {
            let events = Rc::new(RefCell::new(Vec::new()));
            let limits = limits();
            let mut owner = ActiveProcess {
                ops: fake(
                    &events,
                    vec![Err(io::Error::other("injected observation failure"))],
                    false,
                ),
                limits: &limits,
                armed: true,
            };
            let error = finish_for_ending(&mut owner, ending).unwrap_err();
            let failure = error.downcast_ref::<ProcessFailure>().unwrap();
            assert_eq!(failure.ending, Some(ending));
            assert_eq!(failure.kind, TerminationFailure::Observation);
        }
    }

    #[test]
    fn lost_wait_ownership_while_monitoring_disarms_drop() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut owner = ActiveProcess {
            ops: fake(
                &events,
                vec![Err(io::Error::from_raw_os_error(libc::ECHILD))],
                false,
            ),
            limits: &limits,
            armed: true,
        };
        assert!(owner.observe().is_err());
        drop(owner);
        assert_eq!(*events.borrow(), ["observe", "released"]);
    }

    #[test]
    fn all_cleanup_observation_failures_handoff_once_without_more_signals() {
        for (prior, expected) in [
            (0, vec!["observe", "abandon"]),
            (1, vec!["observe", "term", "observe", "abandon"]),
            (
                2,
                vec!["observe", "term", "observe", "kill", "observe", "abandon"],
            ),
        ] {
            let events = Rc::new(RefCell::new(Vec::new()));
            let limits = limits();
            let mut observations: Vec<_> = (0..prior).map(|_| Ok(false)).collect();
            observations.push(Err(io::Error::other("injected observation error")));
            let mut owner = ActiveProcess {
                ops: fake(&events, observations, false),
                limits: &limits,
                armed: true,
            };
            assert!(owner.finish().is_err());
            drop(owner);
            assert_eq!(*events.borrow(), expected);
        }
    }

    #[test]
    fn grace_period_observation_failure_handoffs_without_kill() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = Limits {
            grace: Duration::from_secs(1),
            ..limits()
        };
        let mut owner = ActiveProcess {
            ops: fake(
                &events,
                vec![
                    Ok(false),
                    Err(io::Error::other("injected grace observation")),
                ],
                false,
            ),
            limits: &limits,
            armed: true,
        };
        assert!(owner.finish().is_err());
        drop(owner);
        assert_eq!(*events.borrow(), ["observe", "term", "observe", "abandon"]);
    }

    #[test]
    fn reap_failure_handoffs_once_without_more_signals() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut ops = fake(&events, vec![], false);
        ops.reap_fail = true;
        let mut owner = ActiveProcess {
            ops,
            limits: &limits,
            armed: true,
        };
        assert!(owner.finish().is_err());
        drop(owner);
        assert_eq!(
            *events.borrow(),
            [
                "observe", "term", "observe", "kill", "observe", "reap", "abandon"
            ]
        );
    }

    #[test]
    fn echild_on_reap_never_rewaits_cached_pid() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut ops = fake(&events, vec![], false);
        ops.reap_echild = true;
        let mut owner = ActiveProcess {
            ops,
            limits: &limits,
            armed: true,
        };
        assert!(owner.finish().is_err());
        drop(owner);
        assert_eq!(
            *events.borrow(),
            [
                "observe", "term", "observe", "kill", "observe", "reap", "released"
            ]
        );
    }

    #[test]
    fn real_owned_child_is_reaped_after_each_injected_failure() {
        struct FaultChild {
            inner: OwnedChild,
            events: Rc<RefCell<Vec<&'static str>>>,
            fail_at: Option<usize>,
            observations: usize,
            fail_reap: bool,
        }
        impl ProcessOps for FaultChild {
            fn exited(&mut self) -> io::Result<bool> {
                self.observations += 1;
                if self.fail_at == Some(self.observations) {
                    self.events.borrow_mut().push("failure");
                    return Err(io::Error::other("injected waitid failure"));
                }
                self.inner.exited()
            }
            fn signal(&mut self, signal: i32) -> io::Result<()> {
                self.events.borrow_mut().push("signal");
                self.inner.signal(signal)
            }
            fn reap(&mut self) -> io::Result<ExitStatus> {
                if self.fail_reap {
                    self.events.borrow_mut().push("failure");
                    return Err(io::Error::other("injected reap failure"));
                }
                self.inner.reap()
            }
            fn abandon_wait(&mut self, no_child: bool) -> io::Result<()> {
                self.events.borrow_mut().push("handoff");
                self.inner.abandon_wait(no_child)
            }
        }

        for (monitor, fail_at, fail_reap) in [
            (true, Some(1), false),
            (false, Some(1), false),
            (false, Some(2), false),
            (false, Some(3), false),
            (false, None, true),
        ] {
            let mut command = Command::new("/bin/sleep");
            command.arg("1");
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
            let child = command.spawn().unwrap();
            let pgid = child.id() as i32;
            let (sender, receiver) = std::sync::mpsc::channel::<Child>();
            let (done, completed) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut child = receiver.recv().unwrap();
                let _ = done.send(child.wait());
            });
            let events = Rc::new(RefCell::new(Vec::new()));
            let limits = Limits {
                grace: Duration::ZERO,
                kill_wait: Duration::from_secs(2),
                ..Limits::default()
            };
            let mut owner = ActiveProcess {
                ops: FaultChild {
                    inner: OwnedChild {
                        child: Some(child),
                        pgid,
                        reaper: sender,
                    },
                    events: events.clone(),
                    fail_at,
                    observations: 0,
                    fail_reap,
                },
                limits: &limits,
                armed: true,
            };
            if monitor {
                assert!(owner.observe().is_err());
            } else {
                assert!(owner.finish().is_err());
            }
            drop(owner);
            completed
                .recv_timeout(Duration::from_secs(4))
                .expect("reaper completed")
                .unwrap();
            let events = events.borrow();
            let failure = events.iter().position(|event| *event == "failure").unwrap();
            assert_eq!(&events[failure..], ["failure", "handoff"]);
        }
    }

    #[test]
    fn setup_error_drops_armed_owner() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let setup = || -> Result<()> {
            let _owner = ActiveProcess {
                ops: fake(&events, vec![], false),
                limits: &limits,
                armed: true,
            };
            anyhow::bail!("injected setup failure");
        };
        assert!(setup().is_err());
        assert_eq!(
            *events.borrow(),
            ["observe", "term", "observe", "kill", "observe", "reap"]
        );
    }

    #[test]
    fn failed_signals_and_unknown_group_cannot_report_cleanup_success() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut ops = fake(&events, vec![Ok(false), Ok(false), Ok(true)], false);
        ops.signal_fail = true;
        let mut owner = ActiveProcess {
            ops,
            limits: &limits,
            armed: true,
        };
        assert!(
            owner
                .finish()
                .unwrap_err()
                .to_string()
                .contains("termination unknown")
        );
        drop(owner);
        assert_eq!(
            *events.borrow(),
            ["observe", "term", "observe", "kill", "observe", "reap"]
        );
    }

    #[test]
    fn zombie_group_eperm_preserves_success_with_unknown_survival() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut ops = fake(&events, vec![Ok(true), Ok(true), Ok(true)], false);
        ops.signal_fail = true;
        let mut owner = ActiveProcess {
            ops,
            limits: &limits,
            armed: true,
        };
        let (status, survival) = owner.finish().unwrap();
        assert!(status.success());
        assert_eq!(survival, None);
        drop(owner);
        assert_eq!(
            *events.borrow(),
            ["observe", "term", "observe", "kill", "observe", "reap"]
        );
    }

    #[test]
    fn kill_eperm_after_leader_exit_preserves_forced_end() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut ops = fake(&events, vec![Ok(false), Ok(true), Ok(true)], false);
        ops.signal_errors = vec![None, Some(libc::EPERM)].into();
        let mut owner = ActiveProcess {
            ops,
            limits: &limits,
            armed: true,
        };
        assert_eq!(owner.finish().unwrap().1, None);
        drop(owner);
        assert_eq!(
            *events.borrow(),
            ["observe", "term", "observe", "kill", "observe", "reap"]
        );
    }

    #[test]
    fn signal_error_while_leader_live_remains_typed_failure() {
        for (observations, signals) in [
            (
                vec![Ok(false), Ok(true), Ok(true)],
                vec![Some(libc::EPERM), None],
            ),
            (
                vec![Ok(false), Ok(false), Ok(true)],
                vec![None, Some(libc::EPERM)],
            ),
            (
                vec![Ok(true), Ok(true), Ok(true)],
                vec![Some(libc::EIO), None],
            ),
        ] {
            let events = Rc::new(RefCell::new(Vec::new()));
            let limits = limits();
            let mut ops = fake(&events, observations, false);
            ops.signal_errors = signals.into();
            let mut owner = ActiveProcess {
                ops,
                limits: &limits,
                armed: true,
            };
            let error = owner.finish().unwrap_err();
            assert_eq!(
                error.downcast_ref::<ProcessFailure>().unwrap().kind,
                TerminationFailure::Signal
            );
            drop(owner);
            assert_eq!(
                *events.borrow(),
                ["observe", "term", "observe", "kill", "observe", "reap"]
            );
        }
    }

    #[test]
    fn unexited_leader_is_reaped_later_without_more_signals() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let limits = limits();
        let mut owner = ActiveProcess {
            ops: fake(&events, vec![Ok(false), Ok(false), Ok(false)], false),
            limits: &limits,
            armed: true,
        };
        assert!(
            owner
                .finish()
                .unwrap_err()
                .to_string()
                .contains("leader termination unknown")
        );
        drop(owner);
        assert_eq!(
            *events.borrow(),
            ["observe", "term", "observe", "kill", "observe", "abandon"]
        );
    }

    #[test]
    fn inherited_auto_reap_is_rejected_in_isolated_process() {
        const VAR: &str = "TRELLIS_TEST_SIGCHLD_DISPOSITION_CHILD";
        if let Ok(mode) = std::env::var(VAR) {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            if mode == "ignore" {
                action.sa_sigaction = libc::SIG_IGN;
            } else {
                action.sa_sigaction = libc::SIG_DFL;
                action.sa_flags = libc::SA_NOCLDWAIT;
            }
            assert_eq!(unsafe { libc::sigemptyset(&mut action.sa_mask) }, 0);
            assert_eq!(
                unsafe { libc::sigaction(libc::SIGCHLD, &action, std::ptr::null_mut()) },
                0
            );
            assert!(check_sigchld().is_err());
            return;
        }
        for mode in ["ignore", "no_wait"] {
            let status = Command::new(std::env::current_exe().unwrap())
                .arg("--exact").arg("producer::exec::ownership_tests::inherited_auto_reap_is_rejected_in_isolated_process")
                .env(VAR, mode).status().unwrap();
            assert!(status.success(), "{mode}");
        }
    }

    #[test]
    fn tokio_sigchld_handler_is_accepted() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let _signal =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child()).unwrap();
            check_sigchld().unwrap();
        });
    }

    #[test]
    fn real_owned_short_lived_child() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ProducerSpec {
            argv: vec!["/bin/sh".into(), "-c".into(), "exit 0".into()],
            cwd: dir.path().into(),
            env: BTreeMap::new(),
            artifact: dir.path().join("out"),
            log_dir: None,
        };
        let limits = Limits {
            grace: Duration::from_millis(10),
            kill_wait: Duration::from_millis(100),
            ..Limits::default()
        };
        let result = spawn_producer(
            &spec,
            &limits,
            &AtomicBool::new(false),
            &mut Hooks::default(),
        );
        assert!(result.is_ok(), "{result:?}");
        let outcome = result.unwrap();
        assert_eq!(outcome.diagnostics.exit.code, Some(0));
        assert_eq!(outcome.diagnostics.termination, TerminationStatus::Unknown);
        assert_eq!(outcome.diagnostics.group_survived, None);
        assert_eq!(outcome.diagnostics.stdout_complete, Some(true));
        assert_eq!(outcome.diagnostics.stderr_complete, Some(true));
    }

    #[test]
    fn real_owned_exit_with_lingering_descendant() {
        let dir = tempfile::tempdir().unwrap();
        // The descendant exits on its own in two seconds even if this test
        // fails; the only group signaled is the one created by spawn_producer.
        let spec = ProducerSpec {
            argv: vec![
                "/bin/sh".into(),
                "-c".into(),
                "/bin/sleep 2 & exit 0".into(),
            ],
            cwd: dir.path().into(),
            env: BTreeMap::new(),
            artifact: dir.path().join("out"),
            log_dir: None,
        };
        let limits = Limits {
            grace: Duration::from_millis(10),
            kill_wait: Duration::from_secs(2),
            ..Limits::default()
        };
        let outcome = spawn_producer(
            &spec,
            &limits,
            &AtomicBool::new(false),
            &mut Hooks::default(),
        )
        .unwrap();
        assert_eq!(outcome.ending, Ending::Exited);
        assert_eq!(outcome.diagnostics.exit.code, Some(0));
        assert_eq!(outcome.diagnostics.termination, TerminationStatus::Unknown);
        assert_eq!(outcome.diagnostics.group_survived, None);
    }

    #[test]
    fn real_owned_timeout_preserves_leader_signal() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ProducerSpec {
            argv: vec!["/bin/sleep".into(), "2".into()],
            cwd: dir.path().into(),
            env: BTreeMap::new(),
            artifact: dir.path().join("out"),
            log_dir: None,
        };
        let limits = Limits {
            timeout: Duration::from_millis(20),
            grace: Duration::from_millis(10),
            kill_wait: Duration::from_secs(2),
            ..Limits::default()
        };
        let outcome = spawn_producer(
            &spec,
            &limits,
            &AtomicBool::new(false),
            &mut Hooks::default(),
        )
        .unwrap();
        assert_eq!(outcome.ending, Ending::TimedOut);
        assert_eq!(outcome.diagnostics.exit.signal, Some(libc::SIGTERM));
        assert_eq!(outcome.diagnostics.termination, TerminationStatus::Unknown);
        assert_eq!(outcome.diagnostics.group_survived, None);
    }

    #[test]
    fn real_owned_cancel_terminates_child() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ProducerSpec {
            argv: vec!["/bin/sleep".into(), "2".into()],
            cwd: dir.path().into(),
            env: BTreeMap::new(),
            artifact: dir.path().join("out"),
            log_dir: None,
        };
        let limits = Limits {
            grace: Duration::from_millis(10),
            kill_wait: Duration::from_secs(2),
            ..Limits::default()
        };
        let outcome = spawn_producer(
            &spec,
            &limits,
            &AtomicBool::new(true),
            &mut Hooks::default(),
        )
        .unwrap();
        assert_eq!(outcome.ending, Ending::Cancelled);
        assert_eq!(outcome.diagnostics.exit.signal, Some(libc::SIGTERM));
        assert_eq!(outcome.diagnostics.termination, TerminationStatus::Unknown);
        assert_eq!(outcome.diagnostics.group_survived, None);
    }

    #[test]
    fn real_owned_spawn_hook_unwind_reaps_child() {
        let dir = tempfile::tempdir().unwrap();
        let spec = ProducerSpec {
            argv: vec!["/bin/sleep".into(), "2".into()],
            cwd: dir.path().into(),
            env: BTreeMap::new(),
            artifact: dir.path().join("out"),
            log_dir: None,
        };
        let limits = Limits {
            grace: Duration::from_millis(10),
            kill_wait: Duration::from_secs(2),
            ..Limits::default()
        };
        let mut pid = None;
        let mut hook = |identity: &ProcessIdentity| {
            pid = Some(identity.pgid);
            panic!("hook unwind");
        };
        let error = spawn_producer(
            &spec,
            &limits,
            &AtomicBool::new(false),
            &mut Hooks {
                spawned: Some(&mut hook),
            },
        )
        .unwrap_err();
        assert_eq!(
            error.downcast_ref::<ProcessFailure>().unwrap().kind,
            TerminationFailure::Unwind
        );
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                pid.unwrap() as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        assert_eq!(result, -1);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}

impl<P: ProcessOps> ActiveProcess<'_, P> {
    fn observe(&mut self) -> io::Result<bool> {
        match self.ops.exited() {
            Ok(exited) => Ok(exited),
            Err(error) => {
                // An observation failure cannot prove authority, including ECHILD.
                self.armed = false;
                match self
                    .ops
                    .abandon_wait(error.raw_os_error() == Some(libc::ECHILD))
                {
                    Ok(()) => Err(error),
                    Err(handoff) => Err(io::Error::other(format!(
                        "{error}; child handoff failed: {handoff}"
                    ))),
                }
            }
        }
    }

    fn finish(&mut self) -> Result<(ExitStatus, Option<bool>)> {
        // Disarm first so a failure or unwinding during cleanup cannot repeat it.
        self.armed = false;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.finish_inner())) {
            Ok(result) => result,
            Err(_) => {
                // Cleanup panicked: no second signal attempt. A still-owned
                // child goes to the prestarted, no-signal eventual reaper.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    self.ops.abandon_wait(false)
                }));
                Err(process_failure(
                    TerminationFailure::Unwind,
                    "cleanup unwound",
                ))
            }
        }
    }

    fn finish_inner(&mut self) -> Result<(ExitStatus, Option<bool>)> {
        let term_exited = self
            .observe()
            .map_err(|e| process_failure(TerminationFailure::Observation, e))?;
        let term = self.ops.signal(libc::SIGTERM);
        let now = Instant::now();
        let deadline = now.checked_add(self.limits.grace).unwrap_or(now);
        while Instant::now() < deadline {
            self.observe()
                .map_err(|e| process_failure(TerminationFailure::Observation, e))?;
            std::thread::sleep(
                Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
            );
        }
        let kill_exited = self
            .observe()
            .map_err(|e| process_failure(TerminationFailure::Observation, e))?;
        let kill = self.ops.signal(libc::SIGKILL);
        let now = Instant::now();
        let deadline = now.checked_add(self.limits.kill_wait).unwrap_or(now);
        loop {
            if self
                .observe()
                .map_err(|e| process_failure(TerminationFailure::Observation, e))?
            {
                break;
            }
            if Instant::now() >= deadline {
                self.ops
                    .abandon_wait(false)
                    .map_err(|e| process_failure(TerminationFailure::Timeout, e))?;
                return Err(process_failure(
                    TerminationFailure::Timeout,
                    "leader termination unknown",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // No substantive signal can follow this reap, including on unwind.
        let status = match self.ops.reap() {
            Ok(status) => status,
            Err(error) => {
                let detail = match self
                    .ops
                    .abandon_wait(error.raw_os_error() == Some(libc::ECHILD))
                {
                    Ok(()) => error.to_string(),
                    Err(handoff) => format!("{error}; child handoff failed: {handoff}"),
                };
                return Err(process_failure(TerminationFailure::Reap, detail));
            }
        };
        // Darwin can report EPERM (and legacy variants ESRCH) when its
        // process-group iterator sees only the pinned zombie. This does not
        // prove the group empty; keep survival explicitly unknown. A failure
        // observed while the leader was still live, or another error, remains
        // a typed failure even if the leader subsequently exits.
        let failed = |result: &io::Result<()>, exited: bool| match result {
            Ok(()) => false,
            Err(error) => {
                !exited || !matches!(error.raw_os_error(), Some(libc::EPERM | libc::ESRCH))
            }
        };
        if failed(&term, term_exited) || failed(&kill, kill_exited) {
            return Err(process_failure(
                TerminationFailure::Signal,
                format!("group termination unknown (TERM: {term:?}; KILL: {kill:?})"),
            ));
        }
        Ok((status, None))
    }
}

fn setup_error<P: ProcessOps>(owner: &mut ActiveProcess<'_, P>, name: &str) -> anyhow::Error {
    match owner.finish() {
        Err(error) => error,
        Ok(_) => anyhow::anyhow!("producer_failed: {name}"),
    }
}

impl<P: ProcessOps> Drop for ActiveProcess<'_, P> {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.finish();
        }
    }
}

/// Run one producer to the end of its bounds. The child leads a new session
/// and group; `env_clear` plus `spec.env` is its whole environment. Every
/// ending attempts to terminate the group; unproved survival is reported as
/// `groupSurvived: null`.
pub fn spawn_producer(
    spec: &ProducerSpec,
    limits: &Limits,
    cancel: &AtomicBool,
    hooks: &mut Hooks<'_>,
) -> Result<ProcessOutcome> {
    spawn_producer_with_artifact_size(spec, limits, cancel, hooks, || artifact_len(spec))
}

fn spawn_producer_with_artifact_size(
    spec: &ProducerSpec,
    limits: &Limits,
    cancel: &AtomicBool,
    hooks: &mut Hooks<'_>,
    mut measure_artifact: impl FnMut() -> io::Result<Option<u64>>,
) -> Result<ProcessOutcome> {
    ensure!(!spec.argv.is_empty(), "producer argv is empty");
    check_sigchld().context("producer_failed: SIGCHLD disposition")?;
    let stdout_log = open_log(spec, "stdout")?;
    let stderr_log = open_log(spec, "stderr")?;
    // Reserve the no-signal eventual reaper before spawning: thread creation
    // failure must not strand an owned child when bounded cleanup expires.
    let (reaper, reaping) = std::sync::mpsc::channel::<Child>();
    std::thread::Builder::new()
        .name("producer-reap".into())
        .spawn(move || {
            if let Ok(mut child) = reaping.recv() {
                reap_eventually(|| child.wait(), std::thread::sleep);
            }
        })
        .context("producer_failed: start eventual reaper")?;
    let mut command = Command::new(&spec.argv[0]);
    command
        .args(&spec.argv[1..])
        .env_clear()
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // A new session makes the child its own group leader (group ID == PID), so
    // killpg reaches every descendant that stays in the group. Core dumps of a
    // killed producer must not fill the disk.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            let none = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_CORE, &none) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let started = Instant::now();
    let child = command
        .spawn()
        .with_context(|| format!("spawn {}", spec.argv[0].to_string_lossy()))?;
    let pgid = child.id() as i32;
    let mut owner = ActiveProcess {
        ops: OwnedChild {
            child: Some(child),
            pgid,
            reaper,
        },
        limits,
        armed: true,
    };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let process = ProcessIdentity {
            pgid,
            leader_start_time: process_info::start_time(pgid).unwrap_or(0),
        };
        let stdout_pipe = owner.ops.child.as_mut().unwrap().stdout.take();
        let Some(stdout_pipe) = stdout_pipe else {
            return Err(setup_error(&mut owner, "producer stdout"));
        };
        let stdout = start_reader(stdout_pipe, stdout_log, limits.log_bytes);
        let stderr_pipe = owner.ops.child.as_mut().unwrap().stderr.take();
        let Some(stderr_pipe) = stderr_pipe else {
            return Err(setup_error(&mut owner, "producer stderr"));
        };
        let stderr = start_reader(stderr_pipe, stderr_log, limits.log_bytes);
        if let Some(spawned) = hooks.spawned.as_mut() {
            spawned(&process);
        }

        let deadline = started.checked_add(limits.timeout).unwrap_or(started);
        let tick = TICK.min(limits.sample).max(Duration::from_millis(1));
        let mut next_sample = Instant::now();
        let mut peak_rss = 0_u64;
        let mut ending = Ending::Exited;
        let mut metadata_failed = false;
        loop {
            if owner
                .observe()
                .map_err(|e| process_failure(TerminationFailure::Observation, e))?
            {
                break;
            }
            let now = Instant::now();
            if cancel.load(Ordering::SeqCst) {
                ending = Ending::Cancelled;
                break;
            }
            if now >= deadline {
                ending = Ending::TimedOut;
                break;
            }
            if now >= next_sample {
                next_sample = now.checked_add(limits.sample).unwrap_or(now);
                let rss = process_info::group_resident_bytes(pgid);
                peak_rss = peak_rss.max(rss);
                if rss > limits.memory_bytes {
                    ending = Ending::LimitExceeded(LimitKind::Memory);
                    break;
                }
                match measure_artifact() {
                    Ok(Some(bytes)) if bytes > limits.artifact_bytes => {
                        ending = Ending::LimitExceeded(LimitKind::ArtifactBytes);
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        metadata_failed = true;
                        ending = Ending::ArtifactMetadataUnavailable;
                        break;
                    }
                }
            }
            std::thread::sleep(tick);
        }

        let (status, group_survived) = finish_for_ending(&mut owner, ending)?;
        let artifact_bytes = if metadata_failed {
            None
        } else {
            match measure_artifact() {
                Ok(bytes) => bytes,
                Err(_) => {
                    metadata_failed = true;
                    None
                }
            }
        };
        if metadata_failed {
            if ending == Ending::Exited {
                ending = Ending::ArtifactMetadataUnavailable;
            }
            if ending == Ending::ArtifactMetadataUnavailable && cancel.load(Ordering::SeqCst) {
                ending = Ending::Cancelled;
            }
        }
        // The sample may have missed a short burst: the size after exit is final.
        if ending == Ending::Exited
            && artifact_bytes.is_some_and(|bytes| bytes > limits.artifact_bytes)
        {
            ending = Ending::LimitExceeded(LimitKind::ArtifactBytes);
        }
        let (stdout_bytes, stdout_tail, stdout_truncated, stdout_complete) =
            stdout.finish(limits.log_bytes);
        let (stderr_bytes, stderr_tail, stderr_truncated, stderr_complete) =
            stderr.finish(limits.log_bytes);
        Ok(ProcessOutcome {
            ending,
            process,
            diagnostics: CompleteDiagnostics {
                exit: exit_report(status),
                termination: TerminationStatus::Unknown,
                duration_ms: started.elapsed().as_millis() as u64,
                peak_rss_bytes: peak_rss,
                memory_enforcement: "sampled".to_owned(),
                artifact_bytes,
                stdout_bytes,
                stderr_bytes,
                stdout_tail,
                stderr_tail,
                truncated: stdout_truncated || stderr_truncated,
                stdout_complete: Some(stdout_complete),
                stderr_complete: Some(stderr_complete),
                group_survived,
            },
        })
    }));
    match result {
        Ok(result) => result,
        Err(_) => {
            if owner.armed {
                // The guard is still the unique child owner. Cleanup runs
                // outside the unwind and itself catches cleanup panics.
                let _ = owner.finish();
            } else {
                // A completed reap removes the child; otherwise transfer it
                // without another group signal.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    owner.ops.abandon_wait(false)
                }));
            }
            Err(process_failure(
                TerminationFailure::Unwind,
                "producer run unwound",
            ))
        }
    }
}
