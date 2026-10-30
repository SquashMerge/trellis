//! Private producer state under `<cache>/producers/<root-key>/` (D2): run
//! directories with their `run.json` record, pickup directories, `pickup.json`,
//! phase cleanup, retention and the crash sweep. Paths come from
//! `TopologyRoots::producer_state_dir`.

use super::process_info;
use super::{Diagnostics, PRODUCER_STATE_CORRUPT, ProcessIdentity, TrustProfile, UNSAFE_STATE};
use crate::store::topology::{TopologyRoots, WorkspaceIdentity};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Terminal run records kept per root key.
pub const RETAINED_RUNS: usize = 10;
/// How long the sweep waits for a killed group to empty.
const SWEEP_KILL_WAIT: Duration = Duration::from_secs(10);
/// Directories that exist only while the producer phase is running.
const PHASE_DIRS: [&str; 7] = [
    "worktree",
    "bin",
    "home",
    "tmp",
    "target",
    "producer-config",
    "logs",
];
/// Directories that survive until the generated import request is terminal.
const IMPORT_DIRS: [&str; 2] = ["out", "batch"];

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RunState {
    Materializing,
    Running,
    WritingBatch,
    Importing,
    Done,
    Failed,
    Cancelled,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

/// The daemon that owns a run, identified by PID and start time.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonIdentity {
    pub pid: i32,
    pub start_time: u64,
}

impl DaemonIdentity {
    pub fn current() -> Self {
        let pid = std::process::id() as i32;
        Self {
            pid,
            start_time: process_info::start_time(pid).unwrap_or(0),
        }
    }
}

/// `run.json`: one owner rewrites it atomically at every transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RunRecord {
    pub version: u32,
    pub run_id: String,
    pub producer: String,
    pub trust_profile: TrustProfile,
    pub plan_sha256: String,
    pub source_revision: String,
    pub state: RunState,
    pub outcome: Option<String>,
    pub error_code: Option<String>,
    pub daemon: DaemonIdentity,
    pub process: Option<ProcessIdentity>,
    pub request_id: Option<String>,
    pub created_at: u64,
    pub finished_at: Option<u64>,
    pub diagnostics: Option<Diagnostics>,
}

/// `pickup.json` (D6): at most one entry per producer.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PickupConfig {
    pub version: u32,
    pub entries: Vec<PickupEntry>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PickupEntry {
    pub producer: String,
    pub artifact: String,
    pub last_imported: Option<LastImported>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LastImported {
    pub sha256: String,
    pub source_revision: String,
    pub request_id: Option<String>,
    pub state: String,
    pub error_code: Option<String>,
    pub attempted_at: u64,
}

/// What a sweep did, for the caller's log and for tests.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct SweepReport {
    pub abandoned_runs: Vec<String>,
    pub killed_groups: Vec<i32>,
    pub removed_pickups: Vec<String>,
    /// Runs whose record could not be read for a reason other than
    /// corruption; their directories are left in place.
    pub unreadable_runs: Vec<String>,
}

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn generation_id(id: &str) -> Result<&str> {
    ensure!(
        id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "invalid_request: generation id"
    );
    Ok(id)
}

/// Existing level of producer state: a real directory, owned by the current
/// user, with no group or world access. Unsafe state is refused, never repaired.
fn check_private(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    ensure!(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "{UNSAFE_STATE}"
    );
    Ok(())
}

fn ensure_private(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).with_context(|| format!("create {}", path.display())),
            }
        }
        Err(e) => return Err(e).with_context(|| format!("inspect {}", path.display())),
    }
    check_private(path)
}

fn remove_tree(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => Err(e),
    }
    .or_else(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(e)
        }
    })
    .with_context(|| format!("remove {}", path.display()))
}

/// Temporary file, fsync, rename, directory fsync.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let stage = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&stage)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&stage, dir.join(name))?;
        fs::File::open(dir)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result.with_context(|| format!("write {}", dir.join(name).display()))
}

fn corrupt(what: &str) -> anyhow::Error {
    anyhow::anyhow!("{PRODUCER_STATE_CORRUPT}: {what}")
}

/// The producer state directory of one checkout.
#[derive(Clone, Debug)]
pub struct StateDir {
    path: PathBuf,
}

impl StateDir {
    /// Create the state directory lazily, 0700 at every level below the cache.
    pub fn open(roots: &TopologyRoots, identity: &WorkspaceIdentity) -> Result<Self> {
        roots.reject_root_overlap(identity)?;
        let path = roots.producer_state_dir(identity);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&roots.cache)?;
        ensure_private(&roots.cache.join("producers"))?;
        ensure_private(&path)?;
        Ok(Self { path })
    }

    /// The state directory when it exists and is safe; absent is `None`.
    pub fn existing(roots: &TopologyRoots, identity: &WorkspaceIdentity) -> Result<Option<Self>> {
        let path = roots.producer_state_dir(identity);
        if fs::symlink_metadata(&path).is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            return Ok(None);
        }
        check_private(&roots.cache.join("producers"))?;
        check_private(&path)?;
        Ok(Some(Self { path }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn run_dir(&self, run_id: &str) -> Result<PathBuf> {
        Ok(self.path.join("runs").join(generation_id(run_id)?))
    }

    pub fn pickup_dir(&self, generation: &str) -> Result<PathBuf> {
        Ok(self.path.join("pickups").join(generation_id(generation)?))
    }

    pub fn create_run_dir(&self, run_id: &str) -> Result<PathBuf> {
        let dir = self.run_dir(run_id)?;
        check_private(&self.path)?;
        ensure_private(&self.path.join("runs"))?;
        ensure_private(&dir)?;
        Ok(dir)
    }

    pub fn create_pickup_dir(&self, generation: &str) -> Result<PathBuf> {
        let dir = self.pickup_dir(generation)?;
        check_private(&self.path)?;
        ensure_private(&self.path.join("pickups"))?;
        ensure_private(&dir)?;
        Ok(dir)
    }

    pub fn write_run_record(&self, record: &RunRecord) -> Result<()> {
        let dir = self.run_dir(&record.run_id)?;
        write_atomic(&dir, "run.json", &serde_json::to_vec(record)?)
    }

    /// `None` when the run has no record; a record that fails its closed
    /// parse is `producer_state_corrupt`.
    pub fn read_run_record(&self, run_id: &str) -> Result<Option<RunRecord>> {
        let path = self.run_dir(run_id)?.join("run.json");
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        let record: RunRecord =
            serde_json::from_slice(&bytes).map_err(|_| corrupt("run record"))?;
        ensure!(record.run_id == run_id, corrupt("run record identity"));
        Ok(Some(record))
    }

    /// Every run directory with its record, or the error that made the
    /// record unreadable.
    fn scan_runs(&self) -> Result<Vec<(String, Result<Option<RunRecord>>)>> {
        let runs = self.path.join("runs");
        let entries = match fs::read_dir(&runs) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("list {}", runs.display())),
        };
        let mut out = Vec::new();
        for entry in entries {
            let Ok(name) = entry?.file_name().into_string() else {
                continue;
            };
            if generation_id(&name).is_err() {
                continue;
            }
            let record = self.read_run_record(&name);
            out.push((name, record));
        }
        Ok(out)
    }

    /// Readable run records, newest first. Corrupt records are ignored.
    pub fn runs(&self) -> Result<Vec<RunRecord>> {
        let mut records: Vec<RunRecord> = self
            .scan_runs()?
            .into_iter()
            .filter_map(|(_, record)| record.ok().flatten())
            .collect();
        records.sort_by(|a, b| (b.created_at, &b.run_id).cmp(&(a.created_at, &a.run_id)));
        Ok(records)
    }

    /// The producer phase ended: remove what only the producer needed. The log
    /// tails are already in `run.json`.
    pub fn end_producer_phase(&self, run_id: &str) -> Result<()> {
        let dir = self.run_dir(run_id)?;
        PHASE_DIRS
            .iter()
            .try_for_each(|name| remove_tree(&dir.join(name)))
    }

    /// The import request is terminal: nothing but `run.json` remains.
    pub fn strip_to_record(&self, run_id: &str) -> Result<()> {
        self.end_producer_phase(run_id)?;
        let dir = self.run_dir(run_id)?;
        IMPORT_DIRS
            .iter()
            .try_for_each(|name| remove_tree(&dir.join(name)))
    }

    /// Keep `run.json` for the newest terminal runs and delete the rest.
    pub fn prune_runs(&self) -> Result<()> {
        let mut terminal: Vec<RunRecord> = self
            .runs()?
            .into_iter()
            .filter(|record| record.state.is_terminal())
            .collect();
        for old in terminal.split_off(terminal.len().min(RETAINED_RUNS)) {
            remove_tree(&self.run_dir(&old.run_id)?)?;
        }
        Ok(())
    }

    pub fn remove_pickup_dir(&self, generation: &str) -> Result<()> {
        remove_tree(&self.pickup_dir(generation)?)
    }

    pub fn read_pickup_config(&self) -> Result<Option<PickupConfig>> {
        match fs::read(self.path.join("pickup.json")) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|_| corrupt("pickup configuration")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("read pickup.json"),
        }
    }

    pub fn write_pickup_config(&self, config: &PickupConfig) -> Result<()> {
        check_private(&self.path)?;
        write_atomic(&self.path, "pickup.json", &serde_json::to_vec(config)?)
    }

    pub fn clear_pickup_config(&self) -> Result<()> {
        remove_tree(&self.path.join("pickup.json"))
    }

    /// Recover from a crashed daemon. `keep` holds the generation directories
    /// named by queued or running generated import requests.
    ///
    /// A non-terminal run owned by another daemon is marked
    /// `failed/abandoned` and stripped; its group is killed only when the
    /// recorded leader is still the process that started it. A run directory
    /// whose record fails its closed parse is removed; one that cannot be read
    /// for another reason is left and reported. A pickup
    /// directory that no request names is removed. The inputs an abandoned run
    /// kept are stripped by the first sweep after its request settles.
    pub fn sweep(
        &self,
        this_daemon: &DaemonIdentity,
        keep: &BTreeSet<PathBuf>,
    ) -> Result<SweepReport> {
        let kept = |dir: &Path| {
            keep.contains(dir) || fs::canonicalize(dir).is_ok_and(|real| keep.contains(&real))
        };
        let mut report = SweepReport::default();
        for (run_id, record) in self.scan_runs()? {
            let dir = self.run_dir(&run_id)?;
            let mut record = match record {
                Ok(Some(record)) => record,
                // No record, or one that fails its closed parse, names no
                // group; the directory goes.
                Ok(None) => {
                    if !kept(&dir) {
                        remove_tree(&dir)?;
                    }
                    continue;
                }
                Err(e) if e.to_string().starts_with(PRODUCER_STATE_CORRUPT) => {
                    if !kept(&dir) {
                        remove_tree(&dir)?;
                    }
                    continue;
                }
                // An I/O fault says nothing about the record: leave it and report.
                Err(_) => {
                    report.unreadable_runs.push(run_id);
                    continue;
                }
            };
            if record.state.is_terminal() {
                // An abandoned run kept its import inputs for a pending request; they go
                // once no request names the directory.
                if record.outcome.as_deref() == Some("abandoned") && !kept(&dir) {
                    self.strip_to_record(&run_id)?;
                }
                continue;
            }
            if record.daemon == *this_daemon {
                continue;
            }
            if let Some(process) = record.process
                && leader_matches(&process)
            {
                unsafe { libc::killpg(process.pgid, libc::SIGKILL) };
                let deadline = std::time::Instant::now() + SWEEP_KILL_WAIT;
                while process_info::group_exists(process.pgid)
                    && std::time::Instant::now() < deadline
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                report.killed_groups.push(process.pgid);
            }
            record.state = RunState::Failed;
            record.outcome = Some("abandoned".to_owned());
            record.error_code = Some("abandoned".to_owned());
            record.finished_at = Some(now_millis());
            self.write_run_record(&record)?;
            if kept(&dir) {
                self.end_producer_phase(&run_id)?;
            } else {
                self.strip_to_record(&run_id)?;
            }
            report.abandoned_runs.push(run_id);
        }
        self.prune_runs()?;
        let pickups = self.path.join("pickups");
        if let Ok(entries) = fs::read_dir(&pickups) {
            for entry in entries {
                let entry = entry?;
                if !kept(&entry.path()) {
                    remove_tree(&entry.path())?;
                    report
                        .removed_pickups
                        .push(entry.file_name().to_string_lossy().into_owned());
                }
            }
        }
        Ok(report)
    }
}

/// The recorded group leader is alive and is still the process that started
/// the run, so signalling its group cannot hit a reused process.
fn leader_matches(process: &ProcessIdentity) -> bool {
    process_info::start_time(process.pgid) == Some(process.leader_start_time)
        && unsafe { libc::getpgid(process.pgid) } == process.pgid
}
