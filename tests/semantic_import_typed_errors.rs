//! A failed semantic import reports a typed error code to the CLI caller and in the durable
//! queue row (D5, F10).
#![cfg(feature = "test-support")]
use serde_json::json;
use std::{
    fs,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};
use tempfile::{Builder, TempDir};
use trellis::store::topology::TopologyRoots;

#[path = "support/semantic_batch.rs"]
mod semantic_batch_support;
use semantic_batch_support::{BatchFixture, edit_descriptor, uniform_workspace};

const LANGUAGES: [&str; 4] = ["java", "rust", "python", "javascript"];

fn cli(home: &Path) -> Command {
    let mut cmd = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    cmd.env("HOME", home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("daemon.pid"))
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null());
    cmd
}

fn data_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/dev.squashmerge.trellis")
    } else {
        home.join(".local/share/trellis")
    }
}

fn cache_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Caches/dev.squashmerge.trellis")
    } else {
        home.join(".cache/trellis")
    }
}

/// One indexed checkout with a daemon the first `index` call starts under `env`.
struct World {
    _temp: TempDir,
    home: PathBuf,
    root: PathBuf,
    index_db: PathBuf,
    requests_db: PathBuf,
}

impl World {
    fn new(env: &[(&str, &str)]) -> Self {
        let temp = Builder::new()
            .prefix("semantic-typed-")
            .tempdir_in("/tmp")
            .unwrap();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let root = fs::canonicalize(root).unwrap();
        uniform_workspace(&root, &LANGUAGES, 2);
        fs::write(root.join(".gitignore"), ".trellis-semantic-batch*/\n").unwrap();
        let world = {
            let roots = TopologyRoots::isolated_for_tests(cache_dir(&home), data_dir(&home));
            let identity =
                trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
            let index_db = roots.index_db(&identity);
            let store = trellis::store::Store::open(roots, identity).unwrap();
            let requests_db = store.request_db_path().to_owned();
            drop(store);
            Self {
                _temp: temp,
                home,
                root,
                index_db,
                requests_db,
            }
        };
        let output = world.index(env);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        world
    }

    fn index(&self, env: &[(&str, &str)]) -> Output {
        let mut command = cli(&self.home);
        for (key, value) in env {
            command.env(key, value);
        }
        command
            .args(["index", "--workspace", self.root.to_str().unwrap()])
            .output()
            .unwrap()
    }

    fn fixture(&self) -> BatchFixture {
        let mut fixture = BatchFixture::from_published(&self.index_db, &self.root).synthetic();
        for language in LANGUAGES {
            let docs = fixture.paths(language);
            fixture.artifact(language, docs);
        }
        fixture
    }

    fn import(&self, descriptor: &Path, sha: &str) -> Output {
        cli(&self.home)
            .args([
                "semantic",
                "import",
                "--workspace",
                self.root.to_str().unwrap(),
                "--batch",
                descriptor.to_str().unwrap(),
                "--batch-sha256",
                sha,
            ])
            .output()
            .unwrap()
    }

    /// `(id, state, error_code)` of the last queue row.
    fn last_row(&self) -> (String, String, Option<String>) {
        let db = rusqlite::Connection::open_with_flags(
            &self.requests_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.query_row(
            "SELECT id,state,error_code FROM requests ORDER BY seq DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap()
    }

    fn rows(&self) -> Vec<(String, String, Option<String>)> {
        let db = rusqlite::Connection::open_with_flags(
            &self.requests_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        db.prepare("SELECT id,state,error_code FROM requests ORDER BY seq")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    /// Asserts the import failed, the row holds a code starting with `expected`, and the CLI
    /// reported that same code. Returns the code.
    fn assert_failed_with(&self, output: &Output, expected: &str) -> String {
        assert!(!output.status.success());
        let (_, state, code) = self.last_row();
        assert_eq!(state, "failed");
        let code = code.unwrap();
        assert!(code.starts_with(expected), "row code {code}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(&code), "{stderr}");
        code
    }

    fn stop_daemon(&self) {
        let pid: i32 = fs::read_to_string(self.home.join("daemon.pid"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
        trellis::testing::wait_until("daemon exit", || !trellis::testing::process_running(pid));
        let lock = trellis::daemon::SocketPaths::new(&data_dir(&self.home)).lock;
        trellis::testing::wait_until("daemon election lock released", || {
            let Ok(file) = fs::File::open(&lock) else {
                return true;
            };
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        });
    }
}

impl Drop for World {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(self.home.join("daemon.pid"))
            && let Ok(pid) = pid.parse::<i32>()
        {
            unsafe { libc::kill(pid, libc::SIGTERM) };
        }
    }
}

fn unwatched_batch(world: &World, fixture: &BatchFixture) -> (PathBuf, String) {
    fixture.write(&world.root.join(".trellis-semantic-batch-0"))
}

#[test]
fn failed_import_reports_typed_admission_code() {
    let world = World::new(&[]);
    let fixture = world.fixture();
    let (descriptor, _) = unwatched_batch(&world, &fixture);
    let (descriptor, sha) = edit_descriptor(&descriptor, |value| {
        value["unexpected"] = json!(true);
    });
    let output = world.import(&descriptor, &sha);
    world.assert_failed_with(&output, "invalid_request");
}

#[test]
fn failed_import_reports_typed_artifact_code() {
    let world = World::new(&[]);
    let mut fixture = world.fixture();
    fixture.override_artifact_raw(0, vec![0xff; 24]);
    let (descriptor, sha) = unwatched_batch(&world, &fixture);
    let output = world.import(&descriptor, &sha);
    world.assert_failed_with(&output, "invalid_artifact");
}

#[test]
fn failed_import_reports_limit_code() {
    let world = World::new(&[("TRELLIS_TEST_SEMANTIC_LIMITS", r#"{"descriptor_bytes":64}"#)]);
    let fixture = world.fixture();
    let (descriptor, sha) = unwatched_batch(&world, &fixture);
    let output = world.import(&descriptor, &sha);
    let code = world.assert_failed_with(&output, "limit_exceeded");
    assert_eq!(code, "limit_exceeded: descriptor bytes");
}

/// The CLI rejects an input outside the checkout before queueing it, so no row exists. The
/// claim-time reason spelling is covered by `semantic_input_tests` in `index_coordinator`.
#[test]
fn input_outside_root_reports_reason() {
    let world = World::new(&[]);
    let fixture = world.fixture();
    let (inside, sha) = unwatched_batch(&world, &fixture);
    let outside = world._temp.path().join("outside-batch");
    fs::rename(inside.parent().unwrap(), &outside).unwrap();
    let descriptor = outside.join(inside.file_name().unwrap());
    let before = world.rows();
    let output = world.import(&descriptor, &sha);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("invalid_request: "), "{stderr}");
    assert!(stderr.contains("role outside root"), "{stderr}");
    assert_eq!(world.rows(), before, "nothing was queued");
}

#[test]
fn typed_code_persists_after_restart() {
    let world = World::new(&[]);
    let mut fixture = world.fixture();
    fixture.override_artifact_raw(0, vec![0xff; 24]);
    let (descriptor, sha) = unwatched_batch(&world, &fixture);
    let output = world.import(&descriptor, &sha);
    let code = world.assert_failed_with(&output, "invalid_artifact");
    let failed = world.last_row();
    world.stop_daemon();
    let restarted = world.index(&[]);
    assert!(restarted.status.success());
    let rows = world.rows();
    let row = rows.iter().find(|row| row.0 == failed.0).unwrap();
    assert_eq!(row, &(failed.0.clone(), "failed".to_owned(), Some(code)));
}

#[test]
fn index_request_failure_code_unchanged() {
    let world = World::new(&[]);
    let (_, state, code) = world.last_row();
    assert_eq!(state, "done");
    assert_eq!(code, None);
}
