//! An explicit `trellis daemon stop` cancels accepted work at its next safe
//! boundary instead of draining it (#1293, owner ruling 2026-11-26). Each test
//! holds the work with a daemon hook, so the stop provably arrives first.
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};
use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
use trellis::testing::HANG_BACKSTOP;

pub(crate) struct Fixture {
    _temp: tempfile::TempDir,
    pub(crate) home: PathBuf,
    pub(crate) root: PathBuf,
}

/// A private HOME short enough for the normal socket path, and one Git checkout.
pub(crate) fn fixture() -> Fixture {
    let temp = tempfile::Builder::new()
        .prefix("t1293-")
        .tempdir_in("/tmp")
        .unwrap();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = temp.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("sample.js"), "function sample() { return 1; }\n").unwrap();
    let git = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(git.success(), "git init");
    Fixture {
        _temp: temp,
        home,
        root,
    }
}

impl Fixture {
    /// A Trellis command under this HOME, owned by the test process (#130).
    pub(crate) fn cli(&self) -> Command {
        let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
        command
            .env("HOME", &self.home)
            .env("TRELLIS_TEST_DAEMON_PID_FILE", self.home.join("daemon.pid"))
            .env(crate::child::DAEMON_STDERR, self.home.join("daemon.stderr"))
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdin(Stdio::null());
        command
    }

    /// Start `command` with its output kept under HOME as `<name>.out/.err`.
    pub(crate) fn spawn(&self, name: &str, mut command: Command) -> Proc {
        let child = command
            .stdout(fs::File::create(self.home.join(format!("{name}.out"))).unwrap())
            .stderr(fs::File::create(self.home.join(format!("{name}.err"))).unwrap())
            .spawn()
            .unwrap();
        Proc {
            child,
            err: self.home.join(format!("{name}.err")),
            out: self.home.join(format!("{name}.out")),
        }
    }

    pub(crate) fn daemon(&self, envs: &[(&str, &Path)]) -> Proc {
        let mut command = self.cli();
        command.arg("daemon");
        for (key, value) in envs {
            command.env(key, value);
        }
        let daemon = self.spawn("daemon", command);
        let socket = self.socket();
        trellis::testing::wait_until("the daemon accepts", || {
            UnixStream::connect(&socket).is_ok()
        });
        daemon
    }

    pub(crate) fn socket(&self) -> PathBuf {
        trellis::daemon::SocketPaths::new(&self.roots().data).socket
    }

    pub(crate) fn roots(&self) -> TopologyRoots {
        let (cache, data) = if cfg!(target_os = "macos") {
            (
                self.home.join("Library/Caches/dev.squashmerge.trellis"),
                self.home
                    .join("Library/Application Support/dev.squashmerge.trellis"),
            )
        } else {
            (
                self.home.join(".cache/trellis"),
                self.home.join(".local/share/trellis"),
            )
        };
        TopologyRoots::isolated_for_tests(cache, data)
    }

    /// The state of every accepted request of the checkout, oldest first.
    pub(crate) fn request_states(&self) -> Vec<String> {
        let identity =
            WorkspaceIdentity::discover_unattached(Some(&self.root), &self.root).unwrap();
        let db = rusqlite::Connection::open_with_flags(
            self.roots().requests_db(&identity),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let mut query = db
            .prepare("SELECT state FROM requests ORDER BY seq")
            .unwrap();
        query
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    /// A hook socket under HOME that a daemon connects to.
    pub(crate) fn hook(&self, name: &str) -> (UnixListener, PathBuf) {
        let path = self.home.join(name);
        (UnixListener::bind(&path).unwrap(), path)
    }

    pub(crate) fn status(&self) -> Command {
        let mut command = self.cli();
        command.arg("status").arg("--workspace").arg(&self.root);
        command
    }
}

pub(crate) struct Proc {
    pub(crate) child: Child,
    pub(crate) err: PathBuf,
    pub(crate) out: PathBuf,
}

impl Proc {
    /// Wait for exit within the hang backstop.
    pub(crate) fn exit(&mut self, what: &str) -> ExitStatus {
        let deadline = Instant::now() + HANG_BACKSTOP;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "{what} did not exit (hang)");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    pub(crate) fn stderr(&self) -> String {
        fs::read_to_string(&self.err).unwrap_or_default()
    }
    pub(crate) fn stdout(&self) -> String {
        fs::read_to_string(&self.out).unwrap_or_default()
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Accept the daemon's connection to a hook and read its one-byte arrival.
/// A notifying hook writes and closes at once, and Darwin refuses a socket
/// timeout on a closed peer, so this polls the accept instead.
pub(crate) fn arrival(listener: &UnixListener, expected: u8) -> UnixStream {
    listener.set_nonblocking(true).unwrap();
    let mut accepted = None;
    trellis::testing::wait_until("a daemon hook arrival", || {
        match listener.accept() {
            Ok((stream, _)) => accepted = Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("accept a daemon hook: {error}"),
        }
        accepted.is_some()
    });
    let mut stream = accepted.unwrap();
    // fcntl, unlike SO_RCVTIMEO, also succeeds once the peer closed (#625).
    stream.set_nonblocking(false).unwrap();
    let mut byte = [0];
    stream.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], expected, "unexpected hook arrival");
    stream
}

pub(crate) fn release(mut stream: UnixStream) {
    stream.write_all(&[1]).unwrap();
}

/// Ask the daemon to stop and wait until its stop command reached every
/// checkout runtime, the point where held work must observe it.
pub(crate) fn stop_reaching_runtimes(fixture: &Fixture, stop_hook: &UnixListener) -> Proc {
    let mut command = fixture.cli();
    command.args(["daemon", "stop"]);
    let stop = fixture.spawn("stop", command);
    drop(arrival(stop_hook, b'S'));
    stop
}

#[test]
fn a_stop_cancels_a_held_first_index_which_restarts_on_the_next_activation() {
    let fixture = fixture();
    let (startup, startup_path) = fixture.hook("startup.sock");
    let (stop_hook, stop_path) = fixture.hook("stop.sock");
    let mut daemon = fixture.daemon(&[
        ("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path),
        ("TRELLIS_TEST_DAEMON_STOP_SOCKET", &stop_path),
    ]);
    // `index` activates the checkout and is a mutation, which no client replays.
    let mut index = fixture.cli();
    index.arg("index").arg("--workspace").arg(&fixture.root);
    let mut first = fixture.spawn("index", index);
    let held = arrival(&startup, 1);
    let mut stop = stop_reaching_runtimes(&fixture, &stop_hook);
    // The stop arrived first: the released capture sees it and publishes nothing.
    release(held);
    assert!(stop.exit("daemon stop").success(), "{}", stop.stderr());
    assert!(
        stop.stdout().contains("\"stopped\": true"),
        "{}",
        stop.stdout()
    );
    assert!(daemon.exit("stopped daemon").success());
    assert!(!first.exit("index during the stop").success());

    // The next activation restarts the first index: no head survived the stop.
    let (startup, startup_path) = fixture.hook("startup-2.sock");
    // The first-index wait runs in the daemon: none, so the read answers at once.
    let _daemon = fixture.daemon(&[
        ("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path),
        ("TRELLIS_TEST_CLI_FIRST_INDEX_WAIT_MS", Path::new("0")),
    ]);
    let mut cold = fixture.spawn("status-cold", fixture.status());
    let held = arrival(&startup, 1);
    assert!(!cold.exit("status without a head").success());
    assert!(
        cold.stderr().contains("index_not_ready"),
        "a head was published after the stop: {}",
        cold.stderr()
    );
    release(held);
    let mut after = fixture.spawn("status-after", fixture.status());
    assert!(
        after.exit("status after restart").success(),
        "{}",
        after.stderr()
    );
}

#[test]
fn a_stop_leaves_a_held_claim_running_and_the_next_activation_completes_it() {
    let fixture = fixture();
    let (claim, claim_path) = fixture.hook("claim.sock");
    let (stop_hook, stop_path) = fixture.hook("stop.sock");
    let mut daemon = fixture.daemon(&[
        ("TRELLIS_TEST_DAEMON_INDEX_CLAIM_SOCKET", &claim_path),
        ("TRELLIS_TEST_DAEMON_STOP_SOCKET", &stop_path),
    ]);
    // A published head first, so the FIFO claims the index request.
    let mut ready = fixture.spawn("status-ready", fixture.status());
    assert!(ready.exit("first status").success(), "{}", ready.stderr());
    let mut index = fixture.cli();
    index.arg("index").arg("--workspace").arg(&fixture.root);
    let mut index = fixture.spawn("index", index);
    let held = arrival(&claim, b'C');
    let mut stop = stop_reaching_runtimes(&fixture, &stop_hook);
    release(held);
    assert!(stop.exit("daemon stop").success(), "{}", stop.stderr());
    assert!(daemon.exit("stopped daemon").success());
    assert!(!index.exit("index during the stop").success());
    // Cancelled, not drained: the claimed row is still running, never terminal.
    assert_eq!(fixture.request_states(), ["running"]);

    // The next activation reclaims and completes it (Decision 0008 §3).
    let _daemon = fixture.daemon(&[]);
    let mut status = fixture.spawn("status-after", fixture.status());
    assert!(
        status.exit("status after restart").success(),
        "{}",
        status.stderr()
    );
    trellis::testing::wait_until("the reclaimed request completes", || {
        fixture.request_states() == ["done"]
    });
}
