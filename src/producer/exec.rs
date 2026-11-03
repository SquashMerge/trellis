//! The only place a producer process is started (D8). The child leads its own
//! session and process group; every bound terminates the whole group, and the
//! run's diagnostics report how it ended.

use super::process_info;
use super::{Diagnostics, ExitReport, GroupStatus, ProcessIdentity};
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
    /// A group signal is about to be sent; the leader is still unreaped.
    pub signal: Option<&'a mut dyn FnMut(i32)>,
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

/// The producer's leader, owned until it is reaped (#624). While it is unreaped,
/// running or a zombie, its PID and therefore its group ID cannot be reused, so a
/// group signal reaches only the producer's group. Its exit is observed with
/// `waitid(WNOWAIT)`, which leaves it a zombie; it is reaped only after the last
/// group signal. Nothing else in the process reaps it: no production code waits
/// for any child (`waitpid(-1)`) or installs a SIGCHLD reaper, which a static
/// check enforces, and a spawn is refused while SIGCHLD is ignored.
struct Leader<'h> {
    child: Child,
    pgid: i32,
    /// Reaped, or its wait status can no longer be observed: the group ID may be
    /// reused and the group is never signalled again.
    released: bool,
    kill_wait: Duration,
    on_signal: Option<&'h mut dyn FnMut(i32)>,
}

/// What a group signal returned: `Ok` or the errno.
pub(crate) type Sent = std::result::Result<(), i32>;

impl Leader<'_> {
    /// Whether the leader has exited, without reaping it. An error means its
    /// wait status is gone (another reaper took it): ownership is lost.
    fn exited(&mut self) -> std::io::Result<bool> {
        if self.released {
            return Err(std::io::Error::from_raw_os_error(libc::ECHILD));
        }
        loop {
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.pgid as libc::id_t,
                    &raw mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result == 0 {
                return Ok(unsafe { info.si_pid() } == self.pgid);
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                self.released = true;
                return Err(error);
            }
        }
    }

    /// Signal the group, only while the leader is held.
    fn signal(&mut self, signal: i32) -> Sent {
        if self.released {
            return Err(libc::ESRCH);
        }
        if let Some(hook) = self.on_signal.as_mut() {
            hook(signal);
        }
        if unsafe { libc::killpg(self.pgid, signal) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO))
        }
    }

    /// Wait up to `bound` for the leader to exit; false on timeout or lost ownership.
    fn wait_exit(&mut self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            match self.exited() {
                Ok(true) => return true,
                Err(_) => return false,
                Ok(false) if Instant::now() >= deadline => return false,
                Ok(false) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// Reap the exited leader. The group is never signalled after this.
    fn reap(&mut self) -> Option<ExitStatus> {
        self.released = true;
        self.child.wait().ok()
    }
}

impl Drop for Leader<'_> {
    /// An early return or an unwind while the leader is still held: stop its
    /// group, then reap the leader once it has exited.
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let _ = self.signal(libc::SIGKILL);
        if self.wait_exit(self.kill_wait) {
            self.reap();
        }
    }
}

/// Whether `pgid`'s only listed member is its leader; `None` when the listing
/// does not show the leader, which is held unreaped and so must be listed.
fn only_leader(pgid: i32) -> Option<bool> {
    let members = process_info::group_members(pgid);
    members
        .contains(&pgid)
        .then(|| members.iter().all(|pid| *pid == pgid))
}

/// The group status from the final SIGKILL, whether the leader exited, and the
/// listing after the kill wait. Survival is proven by a live leader or another
/// listed member; an empty group only by a signal that reached the group and a
/// listing of the leader alone. A refused signal (macOS answers EPERM when the
/// only member is the zombie leader) or an unusable listing is unknown.
pub(crate) fn group_status(
    final_kill: Sent,
    leader_exited: bool,
    only: Option<bool>,
) -> GroupStatus {
    match (final_kill, leader_exited, only) {
        (_, false, _) | (_, _, Some(false)) => GroupStatus::Survived,
        (Ok(()), true, Some(true)) => GroupStatus::Empty,
        _ => GroupStatus::Unknown,
    }
}

/// SIGTERM and the grace period unless only the exited leader is left, then the
/// final SIGKILL and the kill wait while the leader is still held, and only then
/// the reap. Returns the leader's status and what was established about the group.
fn terminate(leader: &mut Leader<'_>, limits: &Limits) -> (Option<ExitStatus>, GroupStatus) {
    let pgid = leader.pgid;
    let settled = |leader: &mut Leader<'_>| {
        leader.exited().unwrap_or(false) && only_leader(pgid) == Some(true)
    };
    if !settled(leader) && !leader.released {
        let _ = leader.signal(libc::SIGTERM);
        let deadline = Instant::now() + limits.grace;
        while Instant::now() < deadline && !leader.released && !settled(leader) {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if leader.released {
        return (None, GroupStatus::Unknown);
    }
    let final_kill = leader.signal(libc::SIGKILL);
    // A SIGKILLed leader exits at once; the bound only covers one stuck in the kernel.
    let exited = leader.wait_exit(limits.kill_wait);
    if leader.released {
        return (None, GroupStatus::Unknown);
    }
    let deadline = Instant::now() + limits.kill_wait;
    while only_leader(pgid) == Some(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = group_status(final_kill, exited, only_leader(pgid));
    // A leader that never exited stays held, unreaped.
    if !exited {
        return (None, status);
    }
    let exit = leader.reap();
    // After the reap, a signal-0 probe delivers nothing even to a reused ID, and ESRCH (no
    // process has the group ID) proves the group empty. This settles macOS, whose final
    // SIGKILL to a zombie-only group answers EPERM.
    let probe_esrch = status == GroupStatus::Unknown && !process_info::group_exists(pgid);
    (exit, after_reap(status, probe_esrch))
}

/// The status once the leader is reaped: an unknown group is proven empty only by a probe
/// that found no process with the group ID; any other answer stays unknown.
pub(crate) fn after_reap(status: GroupStatus, probe_esrch: bool) -> GroupStatus {
    match status {
        GroupStatus::Unknown if probe_esrch => GroupStatus::Empty,
        other => other,
    }
}

/// Whether SIGCHLD is ignored or set not to keep zombies: the kernel would then
/// reap the leader itself, and its group ID could not be held.
pub(crate) fn children_auto_reaped(action: &libc::sigaction) -> bool {
    action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0
}

fn sigchld_auto_reaps() -> bool {
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    let read = unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &raw mut action) };
    read == 0 && children_auto_reaped(&action)
}

/// Run one producer to the end of its bounds. The child leads a new session
/// and group; `env_clear` plus `spec.env` is its whole environment. Every
/// ending stops the group while its leader is held and reports what was
/// established about it as `groupStatus`; a descendant that left the group is
/// not tracked.
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
    ensure!(
        !sigchld_auto_reaps(),
        "producer_failed: SIGCHLD is ignored, so the producer group cannot be held"
    );
    let started = Instant::now();
    let child = command
        .spawn()
        .with_context(|| format!("spawn {}", spec.argv[0].to_string_lossy()))?;
    let pgid = child.id() as i32;
    // From here every return or unwind leaves through the leader's owner.
    let mut leader = Leader {
        child,
        pgid,
        released: false,
        kill_wait: limits.kill_wait,
        on_signal: hooks.signal.take(),
    };
    let process = ProcessIdentity {
        pgid,
        leader_start_time: process_info::start_time(pgid).unwrap_or(0),
    };
    let stdout = start_reader(
        leader.child.stdout.take().context("producer stdout")?,
        stdout_log,
        limits.log_bytes,
    );
    let stderr = start_reader(
        leader.child.stderr.take().context("producer stderr")?,
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
    let mut ending = Ending::Exited;
    loop {
        match leader.exited() {
            Ok(true) => break,
            Ok(false) => {}
            // The wait status is gone, so the group ID may already be reused:
            // nothing is signalled (fail closed).
            Err(e) => return Err(e).context("wait for producer"),
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

    // Every ending stops the group while the leader is held, then reaps it.
    let (status, group_status) = terminate(&mut leader, limits);
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
            group_status,
        },
    })
}
