#![cfg(feature = "test-support")]

use serde_json::Value;
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use trellis::store::topology::WorkspaceIdentity;

fn private_dir(path: &Path) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn write_private(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

#[derive(Debug, PartialEq, Eq)]
struct Entry {
    path: PathBuf,
    bytes: Vec<u8>,
    device: u64,
    inode: u64,
    mode: u32,
    len: u64,
    mtime: (i64, i64),
}

// Run only outside the observation interval. Temporarily permit fixture reads of
// mode-000 files, restoring their mode before returning. Atime is not evidence.
fn snapshot(roots: &[PathBuf]) -> Vec<Entry> {
    fn visit(path: &Path, out: &mut Vec<Entry>) {
        let m = fs::symlink_metadata(path).unwrap();
        let bytes = if m.is_file() {
            if m.mode() & 0o444 == 0 {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
                let bytes = fs::read(path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(m.mode())).unwrap();
                bytes
            } else {
                fs::read(path).unwrap()
            }
        } else {
            Vec::new()
        };
        out.push(Entry {
            path: path.to_owned(),
            bytes,
            device: m.dev(),
            inode: m.ino(),
            mode: m.mode(),
            len: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
        });
        if m.is_dir() {
            let mut children = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(&child, out);
            }
        }
    }
    let mut out = Vec::new();
    for root in roots {
        visit(root, &mut out);
    }
    out
}

// Directory watches also observe opens/accesses of their immediate files. All
// subdirectories are known before installation; no fixture walk/read occurs
// between installation and drain. Watching directories permits mode-000 files.
#[cfg(target_os = "linux")]
struct AccessWatch(std::os::fd::OwnedFd);
#[cfg(target_os = "linux")]
impl AccessWatch {
    fn install(entries: &[Entry]) -> Self {
        use std::{ffi::CString, os::fd::FromRawFd, os::unix::ffi::OsStrExt};
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        let watch = Self(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        for entry in entries {
            if entry.mode & libc::S_IFMT == libc::S_IFDIR {
                let path = CString::new(entry.path.as_os_str().as_bytes()).unwrap();
                let wd = unsafe {
                    libc::inotify_add_watch(fd, path.as_ptr(), libc::IN_OPEN | libc::IN_ACCESS)
                };
                assert!(wd >= 0, "{}", std::io::Error::last_os_error());
            }
        }
        watch
    }

    fn assert_no_access(self) {
        use std::os::fd::AsRawFd;
        let mut bytes = [0u8; 4096];
        let count =
            unsafe { libc::read(self.0.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
        assert_eq!(
            count,
            -1,
            "retired-state inotify events (including overflow/watch loss): {:?}",
            &bytes[..count.max(0) as usize]
        );
        assert_eq!(
            std::io::Error::last_os_error().kind(),
            std::io::ErrorKind::WouldBlock
        );
        // Dropping the fd removes watches BEFORE the after-snapshot reads.
    }
}

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
    data: PathBuf,
    key: String,
    record_id: String,
    jev: PathBuf,
    acp: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let workspace = temp.path().join("work");
        private_dir(&home);
        private_dir(&workspace);
        write_private(
            &workspace.join("flow.js"),
            b"export function start() { return send(); }\nfunction send() {}\n",
        );
        let identity = WorkspaceIdentity::discover(Some(&workspace), &workspace).unwrap();
        let data = home.join(if cfg!(target_os = "macos") {
            "Library/Application Support/dev.squashmerge.trellis"
        } else {
            ".local/share/trellis"
        });
        private_dir(&data);
        Self {
            jev: temp.path().join("old-jev"),
            acp: temp.path().join("old-acp"),
            temp,
            home,
            workspace,
            data,
            key: identity.root_key,
            record_id: identity.record_id,
        }
    }

    fn command(&self, sub: &str) -> Command {
        let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
        command
            .env("HOME", &self.home)
            .env_remove("XDG_CACHE_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("JEV_KEY")
            .arg(sub);
        if matches!(sub, "index" | "status" | "symbols" | "query" | "serve") {
            command.arg("--workspace").arg(&self.workspace);
        }
        command
    }

    fn json(&self, label: &str, command: &mut Command) -> Value {
        let stdout = self.temp.path().join(format!("{label}.stdout"));
        let stderr = self.temp.path().join(format!("{label}.stderr"));
        let mut child = Process(
            command
                .stdin(Stdio::null())
                .stdout(fs::File::create(&stdout).unwrap())
                .stderr(fs::File::create(&stderr).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                break status;
            }
            assert!(Instant::now() < deadline, "{label} CLI did not exit");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(status.success(), "{}", fs::read_to_string(stderr).unwrap());
        serde_json::from_slice(&fs::read(stdout).unwrap()).unwrap()
    }

    fn daemon(&self) -> Process {
        let mut child = Process(
            self.command("daemon")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let socket = trellis::daemon::SocketPaths::new(&self.data).socket;
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            assert!(child.0.try_wait().unwrap().is_none(), "daemon exited");
            if std::os::unix::net::UnixStream::connect(&socket).is_ok() {
                return child;
            }
            assert!(Instant::now() < deadline, "daemon did not bind");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn legacy_state(&self) -> Vec<PathBuf> {
        let records = self.data.join("workspaces");
        private_dir(&records);
        let valid = records.join(&self.record_id);
        private_dir(&valid);
        // Literal v1 schema/rows, not a retired saver or topology record helper.
        let db_path = valid.join("workspace.db");
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.execute_batch(
            "PRAGMA user_version=1;
             CREATE TABLE record_metadata(singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema_version INTEGER NOT NULL CHECK(schema_version=1), record_id TEXT NOT NULL, initialized INTEGER NOT NULL CHECK(initialized=1));
             CREATE TABLE known_roots(path TEXT PRIMARY KEY, device TEXT NOT NULL, inode TEXT NOT NULL);
             CREATE TABLE views(id TEXT PRIMARY KEY,payload TEXT NOT NULL);
             CREATE TABLE annotations(id TEXT PRIMARY KEY,node_id TEXT NOT NULL,payload TEXT NOT NULL);",
        )
        .unwrap();
        db.execute(
            "INSERT INTO record_metadata VALUES(1,1,?1,1)",
            [&self.record_id],
        )
        .unwrap();
        let meta = fs::metadata(&self.workspace).unwrap();
        db.execute(
            "INSERT INTO known_roots VALUES(?1,?2,?3)",
            [
                self.workspace.to_str().unwrap().to_owned(),
                meta.dev().to_string(),
                meta.ino().to_string(),
            ],
        )
        .unwrap();
        db.execute(
            "INSERT INTO annotations VALUES('note','node',?1)",
            [r#"{"id":"note","nodeId":"node","body":"keep this note"}"#],
        )
        .unwrap();
        drop(db);
        fs::set_permissions(db_path, fs::Permissions::from_mode(0o600)).unwrap();
        write_private(&records.join(format!("{}.lock", self.record_id)), b"");

        for (id, bytes, mode) in [
            (
                "11111111-1111-4111-8111-111111111111",
                b"not SQLite".as_slice(),
                0o600,
            ),
            (
                "22222222-2222-4222-8222-222222222222",
                b"unreadable old record".as_slice(),
                0o000,
            ),
        ] {
            let dir = records.join(id);
            private_dir(&dir);
            write_private(&dir.join("workspace.db"), bytes);
            for suffix in ["-wal", "-shm", "-journal"] {
                write_private(
                    &dir.join(format!("workspace.db{suffix}")),
                    b"old recovery bytes",
                );
            }
            write_private(&records.join(format!("{id}.lock")), b"old lock bytes");
            fs::set_permissions(dir.join("workspace.db"), fs::Permissions::from_mode(mode))
                .unwrap();
        }
        for (dir, db_name) in [
            (&self.jev, "budget.sqlite3"),
            (&self.acp, "attempts.sqlite3"),
        ] {
            private_dir(dir);
            write_private(&dir.join("initialize.lock"), b"");
            write_private(&dir.join("initialized"), b"old initialization binding");
            write_private(&dir.join(db_name), b"old ledger audit data; not SQLite");
            for suffix in ["-wal", "-shm", "-journal"] {
                write_private(
                    &dir.join(format!("{db_name}{suffix}")),
                    b"old ledger recovery bytes",
                );
            }
            let scratch = dir.join("scratch-33333333-3333-4333-8333-333333333333");
            private_dir(&scratch);
            write_private(&scratch.join("response.json"), b"crash-left audit data");
        }
        fs::set_permissions(
            self.acp.join("attempts.sqlite3"),
            fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        vec![records, self.jev.clone(), self.acp.clone()]
    }
}

#[tokio::test]
async fn ordinary_binary_lifecycle_never_accesses_retired_records_or_ledgers() {
    let fixture = Fixture::new();
    let retired = fixture.legacy_state();
    let before = snapshot(&retired);
    #[cfg(target_os = "linux")]
    let access = AccessWatch::install(&before);

    // Removed configuration is rejected, including exact formerly configured
    // ledger destinations. It is not accepted as a disabled compatibility shim.
    for (flag, path) in [
        ("--jev-budget-dir", &fixture.jev),
        ("--acp-state-dir", &fixture.acp),
    ] {
        let output = fixture
            .command("serve")
            .arg(flag)
            .arg(path)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unexpected argument"));
    }
    let forgotten = fixture
        .command("forget")
        .arg(&fixture.record_id)
        .arg("--yes")
        .output()
        .unwrap();
    assert!(!forgotten.status.success());
    assert!(String::from_utf8_lossy(&forgotten.stderr).contains("unrecognized subcommand"));

    let daemon = fixture.daemon();
    fixture.json("index", &mut fixture.command("index"));
    let status = fixture.json("status", &mut fixture.command("status"));
    assert_eq!(status["stats"]["files"], 1);
    let symbols = fixture.json(
        "symbols",
        fixture.command("symbols").arg("--search").arg("start"),
    );
    let seed = symbols["data"]["items"][0]["id"].as_str().unwrap();
    let query = fixture.json("query", fixture.command("query").arg("--seed").arg(seed));
    assert!(!query["data"]["nodes"].as_array().unwrap().is_empty());

    let log = fixture.temp.path().join("serve.stderr");
    let mut server = Process(
        fixture
            .command("serve")
            .args(["--bind", "127.0.0.1:0"])
            .arg("--token-file")
            .arg(fixture.home.join("token"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let origin = loop {
        let stderr = fs::read_to_string(&log).unwrap();
        if let Some(address) = stderr
            .lines()
            .find_map(|line| line.strip_prefix("Trellis: http://")?.strip_suffix('/'))
        {
            let address: std::net::SocketAddr = address.parse().unwrap();
            assert!(address.ip().is_loopback() && address.port() != 0);
            break format!("http://{address}");
        }
        assert!(
            server.0.try_wait().unwrap().is_none(),
            "serve exited: {stderr}"
        );
        assert!(
            Instant::now() < deadline,
            "serve did not become ready: {stderr}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let token = fs::read_to_string(fixture.home.join("token")).unwrap();
    let client = reqwest::Client::builder()
        .timeout(trellis::testing::HANG_BACKSTOP)
        .build()
        .unwrap();
    for route in [
        "/".to_owned(),
        format!("/api/checkouts/{}/status", fixture.key),
        format!("/api/checkouts/{}/tree", fixture.key),
        format!("/api/checkouts/{}/files", fixture.key),
    ] {
        let response = client
            .get(format!("{origin}{route}"))
            .header("Origin", &origin)
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{route}: {}",
            response.status()
        );
        let _ = response.bytes().await.unwrap();
    }
    drop(server);
    drop(daemon);
    let daemon = fixture.daemon();
    fixture.json("restarted-status", &mut fixture.command("status"));
    // A normal daemon-owned indexing unit after restart also traverses the
    // production activation/reconciliation path, not an in-test Store.
    fixture.json("restarted-index", &mut fixture.command("index"));
    let report = fixture.json("gc", fixture.command("gc").arg("--report"));
    let mut report_keys: Vec<_> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    report_keys.sort_unstable();
    assert_eq!(report_keys, ["derived", "vectors"], "{report}");
    assert_eq!(
        report["vectors"],
        serde_json::json!({"bytes": 0, "entries": 0})
    );
    let derived = report["derived"].as_array().unwrap();
    assert_eq!(derived.len(), 1, "{report}");
    assert_eq!(derived[0]["rootKey"], fixture.key);
    drop(daemon);

    #[cfg(target_os = "linux")]
    access.assert_no_access();
    assert_eq!(
        before,
        snapshot(&retired),
        "retired state must stay in place, byte-identical"
    );
}
