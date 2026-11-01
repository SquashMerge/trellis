//! The only place a producer process is started (D8). The child leads its own
//! session and process group; every bound terminates the whole group, and the
//! run's diagnostics report how it ended.

use super::process_info;
use super::{Diagnostics, ExitReport, ProcessIdentity};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    fs,
    io::{Read, Write},
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
}

#[derive(Debug)]
pub struct ProcessOutcome {
    pub ending: Ending,
    pub process: ProcessIdentity,
    pub diagnostics: Diagnostics,
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
                Ok(0) | Err(_) => return,
                Ok(read) => read,
            };
            let chunk = &buffer[..read];
            let Ok(mut state) = shared.lock() else { return };
            state.total += read as u64;
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
    fn finish(self, cap: u64) -> (u64, String, bool) {
        let deadline = Instant::now() + READER_WAIT;
        while !self.thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let state = self.state.lock().map_or_else(
            |poisoned| {
                let state = poisoned.into_inner();
                (state.total, state.tail.clone())
            },
            |state| (state.total, state.tail.clone()),
        );
        (
            state.0,
            String::from_utf8_lossy(&state.1).into_owned(),
            state.0 > cap,
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

fn artifact_len(spec: &ProducerSpec) -> u64 {
    fs::metadata(&spec.artifact).map_or(0, |meta| meta.len())
}

/// SIGTERM, the grace period, SIGKILL, then wait for the group to empty.
/// Returns the leader's status and whether any group member survived.
fn terminate(child: &mut Child, pgid: i32, limits: &Limits) -> (Option<ExitStatus>, bool) {
    unsafe { libc::killpg(pgid, libc::SIGTERM) };
    let deadline = Instant::now() + limits.grace;
    while Instant::now() < deadline {
        let leader_gone = child.try_wait().ok().flatten().is_some();
        if leader_gone && !process_info::group_exists(pgid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The leader may have exited while descendants still run.
    unsafe { libc::killpg(pgid, libc::SIGKILL) };
    // A SIGKILLed leader is reaped at once; the bound only covers a leader
    // stuck in the kernel.
    let reap_deadline = Instant::now() + limits.kill_wait;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Err(_) => break None,
            Ok(None) if Instant::now() >= reap_deadline => break None,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let deadline = Instant::now() + limits.kill_wait;
    while process_info::group_exists(pgid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    (status, process_info::group_exists(pgid))
}

/// Run one producer to the end of its bounds. The child leads a new session
/// and group; `env_clear` plus `spec.env` is its whole environment. Every
/// ending leaves the group empty unless a descendant escaped it, which is
/// reported as `groupSurvived`.
pub fn spawn_producer(
    spec: &ProducerSpec,
    limits: &Limits,
    cancel: &AtomicBool,
    hooks: &mut Hooks<'_>,
) -> Result<ProcessOutcome> {
    ensure!(!spec.argv.is_empty(), "producer argv is empty");
    let stdout_log = open_log(spec, "stdout")?;
    let stderr_log = open_log(spec, "stderr")?;
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
    let mut child = command
        .spawn()
        .with_context(|| format!("spawn {}", spec.argv[0].to_string_lossy()))?;
    let pgid = child.id() as i32;
    let process = ProcessIdentity {
        pgid,
        leader_start_time: process_info::start_time(pgid).unwrap_or(0),
    };
    let stdout = start_reader(
        child.stdout.take().context("producer stdout")?,
        stdout_log,
        limits.log_bytes,
    );
    let stderr = start_reader(
        child.stderr.take().context("producer stderr")?,
        stderr_log,
        limits.log_bytes,
    );
    if let Some(spawned) = hooks.spawned.as_mut() {
        spawned(&process);
    }

    let deadline = started + limits.timeout;
    let tick = TICK.min(limits.sample).max(Duration::from_millis(1));
    let mut next_sample = Instant::now();
    let mut peak_rss = 0_u64;
    let mut status = None;
    let mut ending = Ending::Exited;
    loop {
        match child.try_wait() {
            Ok(Some(exited)) => {
                status = Some(exited);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                // Leave no group behind even when the leader cannot be polled.
                terminate(&mut child, pgid, limits);
                return Err(e).context("wait for producer");
            }
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
            next_sample = now + limits.sample;
            let rss = process_info::group_resident_bytes(pgid);
            peak_rss = peak_rss.max(rss);
            if rss > limits.memory_bytes {
                ending = Ending::LimitExceeded(LimitKind::Memory);
                break;
            }
            if artifact_len(spec) > limits.artifact_bytes {
                ending = Ending::LimitExceeded(LimitKind::ArtifactBytes);
                break;
            }
        }
        std::thread::sleep(tick);
    }

    // Every exit path ends with the group empty, whether the leader left on
    // its own or was stopped.
    let mut group_survived = false;
    if status.is_none() || process_info::group_exists(pgid) {
        let (final_status, survived) = terminate(&mut child, pgid, limits);
        status = status.or(final_status);
        group_survived = survived;
    }
    let artifact_bytes = artifact_len(spec);
    // The sample may have missed a short burst: the size after exit is final.
    if ending == Ending::Exited && artifact_bytes > limits.artifact_bytes {
        ending = Ending::LimitExceeded(LimitKind::ArtifactBytes);
    }
    let (stdout_bytes, stdout_tail, stdout_truncated) = stdout.finish(limits.log_bytes);
    let (stderr_bytes, stderr_tail, stderr_truncated) = stderr.finish(limits.log_bytes);
    Ok(ProcessOutcome {
        ending,
        process,
        diagnostics: Diagnostics {
            exit: status.map(exit_report).unwrap_or_default(),
            duration_ms: started.elapsed().as_millis() as u64,
            peak_rss_bytes: peak_rss,
            memory_enforcement: "sampled".to_owned(),
            artifact_bytes,
            stdout_bytes,
            stderr_bytes,
            stdout_tail,
            stderr_tail,
            truncated: stdout_truncated || stderr_truncated,
            group_survived,
        },
    })
}
