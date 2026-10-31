#![cfg(feature = "test-support")]
use serde_json::Value;
use std::{fs, process::Command};
use tempfile::TempDir;

/// Only fixed startup markers are retained. Never copy arbitrary daemon stderr,
/// filesystem paths, source text or the bearer into a failure artifact.
fn readiness_stderr_category(sample: &[u8]) -> &'static str {
    let text = String::from_utf8_lossy(sample);
    if text.contains("bind daemon listener") {
        "listener_bind_error"
    } else if text.contains("Error:") {
        "other_startup_error"
    } else if text.contains("Evidence unavailable at startup:") {
        "index_startup_unavailable"
    } else if text.contains("Trellis: http://") {
        "server_banner_present"
    } else {
        "no_allowlisted_marker"
    }
}

/// Opt-in failure-only report, outside the tracked worktree. Every value written
/// here is a fixed category or bounded number, not untrusted process output.
fn write_private_readiness_diagnostic(
    dir: &std::path::Path,
    child: &mut std::process::Child,
    stderr_path: &std::path::Path,
    address: std::net::SocketAddr,
    healthz: &str,
) -> std::io::Result<()> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let meta = fs::symlink_metadata(dir)?;
    if !dir.is_absolute()
        || !meta.is_dir()
        || meta.file_type().is_symlink()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o700
        || dir
            .canonicalize()?
            .starts_with(std::env::current_dir()?.canonicalize()?)
    {
        return Err(std::io::Error::other(
            "unsafe readiness diagnostic directory",
        ));
    }
    let child_state = match child.try_wait()? {
        Some(status) => match status.code() {
            Some(code) => format!("exited_code_{code}"),
            None => {
                use std::os::unix::process::ExitStatusExt;
                format!("exited_signal_{}", status.signal().unwrap_or_default())
            }
        },
        None => "alive_at_deadline".to_owned(),
    };
    let listener =
        match std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(150))
        {
            Ok(_) => "tcp_accepts",
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => "tcp_refused",
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => "tcp_timeout",
            Err(_) => "tcp_other_error",
        };
    let mut sample = Vec::new();
    fs::File::open(stderr_path)?
        .take(16_384)
        .read_to_end(&mut sample)?;
    let category = readiness_stderr_category(&sample);
    let output = dir.join(format!(
        "queue-api-readiness-{}-{}.log",
        std::process::id(),
        child.id()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(output)?;
    // The caller supplies `healthz` from a closed set of literals or HTTP status
    // numbers. Do not accept arbitrary reqwest error text here.
    writeln!(
        file,
        "child={child_state} healthz={healthz} listener={listener} stderr_category={category}"
    )?;
    file.sync_all()?;
    Ok(())
}

#[test]
fn readiness_stderr_category_never_copies_secret_or_temp_path() {
    let poisoned = b"Error: bind daemon listener /private/tmp/secret Bearer 0123456789abcdef\n";
    let category = readiness_stderr_category(poisoned);
    assert_eq!(category, "listener_bind_error");
    assert!(!category.contains("Bearer"));
    assert!(!category.contains("/private"));
    assert!(!category.contains("0123456789abcdef"));
    let mixed = b"Trellis: http://127.0.0.1:7331/\nEvidence unavailable at startup: /private/tmp/secret\nError: failed after banner; Bearer 0123456789abcdef\n";
    let fatal = readiness_stderr_category(mixed);
    assert_eq!(fatal, "other_startup_error");
    assert!(!fatal.contains("Bearer"));
    assert!(!fatal.contains("/private"));
    assert!(!fatal.contains("0123456789abcdef"));
}

#[test]
fn private_readiness_report_is_mode_600_bounded_and_never_copies_raw_stderr() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("private");
    fs::create_dir(&dir).unwrap();
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    let stderr_path = tmp.path().join("daemon-stderr");
    let secret = "Bearer 0123456789abcdef /private/tmp/user-workspace source-data";
    fs::write(
        &stderr_path,
        format!("Error: bind daemon listener {secret}\n"),
    )
    .unwrap();
    let mut child = Command::new("true").spawn().unwrap();
    write_private_readiness_diagnostic(
        &dir,
        &mut child,
        &stderr_path,
        "127.0.0.1:9".parse().unwrap(),
        "connect_error",
    )
    .unwrap();
    let _ = child.wait();
    let path = fs::read_dir(&dir).unwrap().next().unwrap().unwrap().path();
    let report = fs::read_to_string(&path).unwrap();
    assert!(report.len() < 256);
    assert!(report.contains("stderr_category=listener_bind_error"));
    assert!(!report.contains(secret));
    assert!(!report.contains("Bearer"));
    assert!(!report.contains("/private/tmp"));
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

fn connected_peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "macos")]
    unsafe {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        let rc = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut len,
        );
        (rc == 0 && len as usize == std::mem::size_of_val(&pid) && pid > 0).then_some(pid as u32)
    }
    #[cfg(target_os = "linux")]
    unsafe {
        let mut cred: libc::ucred = std::mem::zeroed();
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        let rc = libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        );
        (rc == 0 && len as usize == std::mem::size_of_val(&cred) && cred.pid > 0)
            .then_some(cred.pid as u32)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = stream;
        None
    }
}

/// Test-scope guard for only the daemon elected under this disposable HOME.
struct FixtureDaemon(std::path::PathBuf);
impl FixtureDaemon {
    fn new(home: &std::path::Path) -> Self {
        Self(home.to_path_buf())
    }

    fn stop_attested(&self) -> Result<(), &'static str> {
        let marker = self.0.join("auto-daemon.pid");
        let raw = fs::read_to_string(&marker).map_err(|_| "pid_marker_missing")?;
        let pid = raw.parse::<u32>().map_err(|_| "pid_marker_invalid")?;
        let mut dirs = vec![self.0.clone()];
        let mut socket = None;
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.file_name().is_some_and(|name| name == "daemon.lock") {
                    socket = path
                        .parent()
                        .and_then(std::path::Path::parent)
                        .map(|data| trellis::daemon::SocketPaths::new(data).socket);
                    break;
                }
                if path.is_dir() {
                    dirs.push(path);
                }
            }
            if socket.is_some() {
                break;
            }
        }
        let socket = socket.ok_or("daemon_lock_missing")?;
        let connection = std::os::unix::net::UnixStream::connect(&socket)
            .map_err(|_| "daemon_socket_unavailable")?;
        if connected_peer_pid(&connection) != Some(pid) {
            return Err("daemon_peer_pid_mismatch");
        }
        let output = Command::new("/bin/ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .map_err(|_| "daemon_ps_failed")?;
        if !output.status.success()
            || String::from_utf8_lossy(&output.stdout).trim()
                != format!("{} daemon", env!("CARGO_BIN_EXE_trellis"))
        {
            return Err("daemon_command_mismatch");
        }
        let sent = Command::new("/bin/kill")
            .args(["-TERM", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        if !sent {
            return Err("daemon_kill_failed");
        }
        let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
        while std::time::Instant::now() < deadline {
            if std::os::unix::net::UnixStream::connect(&socket).is_err() {
                let _ = fs::remove_file(&marker);
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // Never SIGKILL a numeric PID after the socket identity becomes ambiguous.
        Err("daemon_socket_still_active")
    }
}
impl Drop for FixtureDaemon {
    fn drop(&mut self) {
        let _ = self.stop_attested();
    }
}

fn isolated_command(home: &std::path::Path) -> Command {
    let mut cmd = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    // ProjectDirs uses inherited XDG roots before HOME on Linux.
    cmd.env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME");
    cmd
}

fn command(root: &std::path::Path, state: &std::path::Path, sub: &str) -> Command {
    let mut c = isolated_command(state);
    // Selected graph oracles must first stop the daemon they would otherwise
    // compete with; record its test-scoped identity for an attested stop.
    c.env(
        "TRELLIS_TEST_DAEMON_PID_FILE",
        state.join("auto-daemon.pid"),
    );
    c.arg(sub).arg("--workspace").arg(root);
    if sub == "serve" {
        c.arg("--token-file").arg(state.join("token"));
    }
    c
}

// A spawned daemon can inherit the CLI's stdout/stderr descriptors. A failed
// CLI must not wait for that descendant to close Command::output pipes.
fn bounded_invalid_cli(
    dir: &std::path::Path,
    phase: &str,
    command: &mut Command,
) -> std::process::Output {
    use std::process::{Output, Stdio};
    use std::time::{Duration, Instant};
    let stdout = dir.join(format!("invalid-{phase}.stdout"));
    let stderr = dir.join(format!("invalid-{phase}.stderr"));
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("invalid phase={phase} CLI did not exit within 20 seconds");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    Output {
        status,
        stdout: fs::read(stdout).unwrap(),
        stderr: fs::read(stderr).unwrap(),
    }
}

#[test]
fn unchanged_serve_reuses_selected_versions_and_detects_selected_source_corruption() {
    use std::{os::unix::fs::MetadataExt, time::Duration};
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn serve(root: &std::path::Path, home: &std::path::Path, log: &std::path::Path) -> Server {
        // One daemon owns the requested bind for its lifetime. Ask it to
        // allocate the port instead of releasing a reserved free port first.
        let requested_bind = "127.0.0.1:0";
        let child = command(root, home, "serve")
            .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
            .arg("--bind")
            .arg(requested_bind)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::from(fs::File::create(log).unwrap()))
            .spawn()
            .unwrap();
        let mut server = Server(child);
        // The CLI may spend 5s connecting and 10s awaiting the daemon reply
        // before printing the bound address. The test still fails boundedly.
        let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let stderr = fs::read_to_string(log).unwrap();
            if let Some(raw) = stderr
                .lines()
                .find_map(|line| line.strip_prefix("Trellis: http://")?.strip_suffix('/'))
            {
                let address: std::net::SocketAddr = raw.parse().expect("serve banner address");
                assert!(address.ip().is_loopback() && address.port() != 0);
                if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100))
                    .is_ok()
                {
                    assert!(home.join("auto-daemon.pid").exists());
                    return server;
                }
            }
            if let Some(status) = server.0.try_wait().unwrap() {
                panic!("Serve exited {status}: {stderr}");
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Serve not ready: {stderr}"
            );
            std::thread::sleep(Duration::from_millis(40));
        }
    }
    type RevisionProjectionIds = (i64, i64, i64, Option<i64>);
    fn pin_and_ids(db: &rusqlite::Connection) -> (i64, Vec<RevisionProjectionIds>) {
        let rev = db
            .query_row("SELECT index_revision FROM index_metadata", [], |r| {
                r.get(0)
            })
            .unwrap();
        let mut statement = db
            .prepare(
                "SELECT h.published_index_revision,m.vid,m.gid,m.cid
             FROM revision_documents m JOIN native_revisions h ON h.id=m.revision_id
             ORDER BY h.published_index_revision,m.path",
            )
            .unwrap();
        let ids = statement
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        (rev, ids)
    }
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let _daemon = FixtureDaemon::new(&home);
    fs::create_dir(&root).unwrap();
    let file = root.join("a.js");
    fs::write(&file, "function seed() { return 42; }\n").unwrap();
    let first = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let dbpath = real_index_db(&home);
    let db = rusqlite::Connection::open(&dbpath).unwrap();
    let (r1, old_ids) = pin_and_ids(&db);
    // Startup reconciliation publishes r1; the unchanged Q completes at it.
    assert_eq!(r1, 1);
    let immutable = [
        "document_versions",
        "graph_projections",
        "class_projections",
        "graph_nodes",
        "graph_display_labels",
        "classes",
        "class_relations",
        "native_version_declarations",
        "native_version_calls",
    ];
    let old_counts: Vec<i64> = immutable
        .iter()
        .map(|table| {
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap()
        })
        .collect();
    let queue = dbpath.with_file_name("requests.db");
    let queue_before = fs::read(&queue).unwrap();
    drop(db);

    let first_log = temp.path().join("serve-unchanged.log");
    let leader = serve(&root, &home, &first_log);
    // Serve registers in place on the running checkout (Decision 0009 §6): no
    // replacement runtime and no new startup reconciliation, so an unchanged
    // workspace publishes nothing.
    let attached = command(&root, &home, "status").output().unwrap();
    assert!(
        attached.status.success(),
        "{}",
        String::from_utf8_lossy(&attached.stderr)
    );
    let db = rusqlite::Connection::open(&dbpath).unwrap();
    let (r2, ids) = pin_and_ids(&db);
    assert_eq!(r2, 1, "unchanged serve publishes no new pin");
    assert_eq!(ids, old_ids, "unchanged serve adds no manifest");
    for (table, count) in immutable.iter().zip(old_counts.iter()) {
        let after: i64 = db
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after, *count,
            "{table} gained immutable rows on unchanged Serve"
        );
    }
    assert_eq!(fs::read(&queue).unwrap(), queue_before);
    assert!(
        !fs::read_to_string(&first_log)
            .unwrap()
            .contains("Evidence unavailable")
    );
    let status = command(&root, &home, "status").output().unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&status.stdout).unwrap()["revision"]["indexRevision"],
        1
    );
    drop(db);
    drop(leader);

    let old_stat = fs::metadata(&file).unwrap();
    let old_bytes = fs::read(&file).unwrap();
    fs::write(
        &file,
        String::from_utf8(old_bytes.clone())
            .unwrap()
            .replace("seed", "sued"),
    )
    .unwrap();
    use std::os::unix::ffi::OsStrExt;
    let cpath = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
    let timestamps = [
        libc::timespec {
            tv_sec: old_stat.atime(),
            tv_nsec: old_stat.atime_nsec(),
        },
        libc::timespec {
            tv_sec: old_stat.mtime(),
            tv_nsec: old_stat.mtime_nsec(),
        },
    ];
    assert_eq!(
        unsafe { libc::utimensat(libc::AT_FDCWD, cpath.as_ptr(), timestamps.as_ptr(), 0) },
        0
    );
    assert_eq!(
        fs::metadata(&file).unwrap().mtime_nsec(),
        old_stat.mtime_nsec()
    );
    assert_eq!(fs::read(&file).unwrap().len(), old_bytes.len());
    let changed_log = temp.path().join("serve-changed.log");
    let changed = serve(&root, &home, &changed_log);
    // A repeated serve registration is inert. Explicit index owns a deliberate
    // same-mtime edit; a status read cannot publish it on demand.
    let published = command(&root, &home, "index").output().unwrap();
    assert!(
        published.status.success(),
        "{}",
        String::from_utf8_lossy(&published.stderr)
    );
    let result: Value = serde_json::from_slice(&published.stdout).unwrap();
    let published_rev = result["publishedRevision"]["indexRevision"]
        .as_i64()
        .unwrap();
    assert!(
        matches!(published_rev, 2 | 3),
        "one Q publication, with at most one watcher publication of the edit: {result}"
    );
    let attached = command(&root, &home, "status").output().unwrap();
    assert!(
        attached.status.success(),
        "{}",
        String::from_utf8_lossy(&attached.stderr)
    );
    // Restart, reconcile and watcher publication share a loaded verify host;
    // a 10 s bound failed intermittently (#131).
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    while rusqlite::Connection::open(&dbpath)
        .map(|db| pin_and_ids(&db).0)
        .unwrap()
        < published_rev
    {
        assert!(
            std::time::Instant::now() < deadline,
            "changed bytes did not publish"
        );
        let attached = command(&root, &home, "status").output().unwrap();
        assert!(
            attached.status.success(),
            "{}",
            String::from_utf8_lossy(&attached.stderr)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let db = rusqlite::Connection::open(&dbpath).unwrap();
    let (r3, next_ids) = pin_and_ids(&db);
    assert_eq!(r3, published_rev, "completed Q must name the selected head");
    assert_eq!(next_ids.len(), usize::try_from(r3).unwrap());
    assert_eq!(
        &next_ids[..ids.len()],
        ids.as_slice(),
        "retained prior pins changed"
    );
    let changed_ids = &next_ids[ids.len()];
    assert_ne!(
        ids.last().unwrap().1,
        changed_ids.1,
        "same-length/mtime edit reused stale bytes"
    );
    for current in next_ids.iter().skip(ids.len()) {
        assert_eq!(
            (&current.1, &current.2, &current.3),
            (&changed_ids.1, &changed_ids.2, &changed_ids.3),
            "the watcher and Q must select the same new immutable evidence"
        );
    }
    assert_eq!(next_ids[0].1, old_ids[0].1, "retained old pin changed");
    drop(db);
    drop(changed);

    let db = rusqlite::Connection::open(&dbpath).unwrap();
    let selected_id = &next_ids.last().unwrap().1;
    assert_eq!(
        db.execute(
            "UPDATE document_versions SET source_bytes=?1 WHERE vid=?2",
            rusqlite::params![vec![b'x'; old_bytes.len()], selected_id]
        )
        .unwrap(),
        1
    );
    drop(db);
    let corrupt_log = temp.path().join("serve-corrupt.log");
    let rejected = serve(&root, &home, &corrupt_log);
    // Attachment starts startup reconciliation; a selected read must still refuse the damaged head.
    let corrupt_selected = command(&root, &home, "symbols").output().unwrap();
    assert!(
        !corrupt_selected.status.success(),
        "corrupt selected corrupt was accepted"
    );
    assert!(
        corrupt_selected.stdout.is_empty(),
        "damaged selected corrupt escaped on stdout"
    );
    let db = rusqlite::Connection::open(&dbpath).unwrap();
    assert_eq!(
        pin_and_ids(&db),
        (r3, next_ids.clone()),
        "corrupt selected source was republished"
    );
    let headers: i64 = db
        .query_row("SELECT count(*) FROM native_revisions", [], |r| r.get(0))
        .unwrap();
    let bindings: i64 = db
        .query_row("SELECT count(*) FROM revision_producer_bindings", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        (headers, bindings),
        (r3, r3),
        "refused Serve installed partial metadata"
    );
    drop(db);
    drop(rejected);
}

#[test]
fn daemon_status_uses_selected_head_and_reports_typed_contention() {
    use std::os::fd::AsRawFd;
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function seed() {}\n").unwrap();
    let indexed = command(&root, &home, "index").output().unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let pin =
        serde_json::from_slice::<Value>(&indexed.stdout).unwrap()["publishedRevision"].clone();
    let status = command(&root, &home, "status").output().unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["revision"], pin);
    assert!(status["catchingUp"].is_boolean());
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let indexes = home.join(if cfg!(target_os = "macos") {
        "Library/Caches/dev.squashmerge.trellis/indexes"
    } else {
        ".cache/trellis/indexes"
    });
    let lock = fs::OpenOptions::new()
        .read(true)
        .open(indexes.join(format!("{}.lock", identity.root_key)))
        .unwrap();
    // The daemon retains its index-use lock while serving; an external GC
    // contender never gets an unguarded partial result.
    assert_ne!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let report = isolated_command(&home)
        .args(["gc", "--report"])
        .output()
        .unwrap();
    assert!(
        report.status.success(),
        "gc --report failed: {}\nstdout: {}\nstderr: {}",
        report.status,
        String::from_utf8_lossy(&report.stdout),
        String::from_utf8_lossy(&report.stderr)
    );
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["derived"][0]["status"], "busy");
}

#[test]
fn cli_helper_uses_isolated_home_instead_of_inherited_xdg_roots() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();

    let mut cmd = command(&workspace, &home, "status");
    let overrides = cmd.get_envs().collect::<Vec<_>>();
    for key in ["XDG_CACHE_HOME", "XDG_DATA_HOME"] {
        assert!(
            overrides
                .iter()
                .any(|(name, value)| *name == key && value.is_none()),
            "{key} must be removed from the child environment"
        );
    }
    let absent = cmd.output().unwrap();
    assert!(
        absent.status.success(),
        "{}",
        String::from_utf8_lossy(&absent.stderr)
    );
    let first: Value = serde_json::from_slice(&absent.stdout).unwrap();
    assert!(first["revision"]["indexRevision"].as_u64().unwrap() >= 1);
    let indexed = command(&workspace, &home, "index").output().unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let output = command(&workspace, &home, "status").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(status["stats"]["files"], 0);
    // The unchanged request completes at the isolated home's current pair.
    let indexed: Value = serde_json::from_slice(&indexed.stdout).unwrap();
    assert_eq!(indexed["publishedRevision"], status["revision"]);
    assert_eq!(status["revision"], first["revision"]);
    let cache = home.join(if cfg!(target_os = "macos") {
        "Library/Caches/dev.squashmerge.trellis"
    } else {
        ".cache/trellis"
    });
    assert!(cache.exists());
}

#[test]
fn daemon_index_records_nondefault_file_cap() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("boundary.js"), "function boundary() {}\n").unwrap();

    let indexed = command(&root, &home, "index")
        .arg("--max-file-bytes")
        .arg("3145728")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let indexed: Value = serde_json::from_slice(&indexed.stdout).unwrap();
    assert_eq!(indexed["status"]["stats"]["files"], 1);
    let indexes = if cfg!(target_os = "macos") {
        home.join("Library/Caches/dev.squashmerge.trellis/indexes")
    } else {
        home.join(".cache/trellis/indexes")
    };
    let db_path = fs::read_dir(indexes)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let db = rusqlite::Connection::open(&db_path).unwrap();
    let options: String = db
        .query_row(
            "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let options: Value = serde_json::from_str(&options).unwrap();
    assert_eq!(options["maxFileBytes"], 3_145_728);
    drop(db);

    let status = command(&root, &home, "status").output().unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["revision"], indexed["publishedRevision"]);
    assert_eq!(status["stats"]["files"], 1);
}

#[test]
fn index_rejects_removed_scip_flags_before_request() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("main.js"), "function f() {}\n").unwrap();
    for flag in ["--scip", "--manifest"] {
        let output = command(&root, &temp.path().join("home"), "index")
            .arg(flag)
            .arg("artifact")
            .output()
            .unwrap();
        assert!(!output.status.success(), "removed {flag} must not index");
    }
}

#[tokio::test]
async fn serve_rejects_removed_optional_artifact_flags() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("main.js"), "function f() {}\n").unwrap();
    for flag in ["--scip", "--manifest"] {
        let output = command(&root, &temp.path().join("home"), "serve")
            .arg(flag)
            .arg("artifact")
            .output()
            .unwrap();
        assert!(!output.status.success(), "removed {flag} must not serve");
    }
}

#[test]
fn explicit_cli_index_preserves_custom_options_after_bounded_read_recovery() {
    use std::os::unix::fs::MetadataExt;
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let first = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .arg("--max-file-bytes")
        .arg("2097152")
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert!(home.join("auto-daemon.pid").exists());
    // The old daemon must release its live SQLite handles before the fixture
    // corrupts this disposable index inode. The guard attests its socket peer.
    FixtureDaemon::new(&home).stop_attested().unwrap();
    let indexes = if cfg!(target_os = "macos") {
        home.join("Library/Caches/dev.squashmerge.trellis/indexes")
    } else {
        home.join(".cache/trellis/indexes")
    };
    let dir = fs::read_dir(&indexes)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    let index = dir.join("index.db");
    let use_lock = dir.parent().unwrap().join(format!(
        "{}.lock",
        dir.file_name().unwrap().to_string_lossy()
    ));
    let old_inode = fs::symlink_metadata(&use_lock).unwrap();
    // The prior daemon released the index-use lock before fixture corruption.
    drop(
        trellis::store::topology::UseGuard::acquire_existing_readonly_exclusive(&use_lock)
            .expect("old daemon retained index-use lock"),
    );
    fs::write(&index, b"invalid-sqlite-header-with-at-least-20-bytes").unwrap();
    let queue = rusqlite::Connection::open(dir.join("requests.db")).unwrap();
    let prior_requests: Vec<(String, String, String)> = queue
        .prepare("SELECT id,state,options_json FROM requests ORDER BY seq")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    drop(queue);
    assert_eq!(prior_requests.len(), 1);
    assert_eq!(prior_requests[0].1, "done");
    for sub in ["status", "symbols", "query"] {
        let mut cmd = command(&root, &home, sub);
        cmd.env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"));
        match sub {
            "symbols" => {
                cmd.arg("--search").arg("a");
            }
            "query" => {
                cmd.arg("--seed").arg("unknown");
            }
            _ => {}
        }
        let read = cmd.output().unwrap();
        if read.status.success() {
            serde_json::from_slice::<Value>(&read.stdout)
                .unwrap_or_else(|error| panic!("{sub} emitted invalid evidence: {error}"));
        } else {
            assert!(read.stdout.is_empty(), "{sub} emitted unverified evidence");
            let reason = String::from_utf8_lossy(&read.stderr);
            assert!(
                reason.contains("recovery_required")
                    || reason.contains("index_not_ready")
                    || (sub == "query" && reason.contains("seed not found in current index")),
                "{sub}: {reason}"
            );
        }
    }
    // A bounded read may have elected a new daemon. Retire only its attested
    // socket peer before the cold exceptional-recovery index operation.
    FixtureDaemon::new(&home).stop_attested().unwrap();
    drop(
        trellis::store::topology::UseGuard::acquire_existing_readonly_exclusive(&use_lock)
            .expect("read daemon retained index-use lock"),
    );
    let recovered = command(&root, &home, "index")
        .arg("--max-file-bytes")
        .arg("2097152")
        .output()
        .unwrap();
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    let output: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(output["publishedRevision"], output["status"]["revision"]);
    // A bounded read can already have recreated with configured defaults.
    assert!(
        output["publishedRevision"]["indexRevision"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_ne!(
        output["publishedRevision"]["indexGeneration"],
        first["publishedRevision"]["indexGeneration"]
    );
    let new_inode = fs::symlink_metadata(&use_lock).unwrap();
    assert_eq!(
        (old_inode.dev(), old_inode.ino()),
        (new_inode.dev(), new_inode.ino())
    );
    let db = rusqlite::Connection::open(&index).unwrap();
    let recorded: String = db
        .query_row(
            "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&recorded).unwrap()["maxFileBytes"],
        2_097_152
    );
    let queue = rusqlite::Connection::open(dir.join("requests.db")).unwrap();
    for (id, state, options) in prior_requests {
        let retained: (String, String) = queue
            .query_row(
                "SELECT state,options_json FROM requests WHERE id=?1",
                [&id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            retained,
            (state, options),
            "terminal request {id} changed during recreation"
        );
    }
    let done: i64 = queue.query_row(
        "SELECT count(*) FROM requests WHERE state='done' AND json_extract(options_json,'$.maxFileBytes')=2097152", [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(
        done, 2,
        "both accepted custom requests must remain completed"
    );
}

#[test]
fn corrupt_read_activation_then_accepted_cli_options_share_the_same_daemon() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let marker = home.join("auto-daemon.pid");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let first = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &marker)
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    FixtureDaemon::new(&home).stop_attested().unwrap();
    let index = real_index_db(&home);
    let dir = index.parent().unwrap();
    let use_lock = dir.parent().unwrap().join(format!(
        "{}.lock",
        dir.file_name().unwrap().to_string_lossy()
    ));
    // The socket can close before all retained SQLite handles are gone.
    drop(
        trellis::store::topology::UseGuard::acquire_existing_readonly_exclusive(&use_lock)
            .expect("old daemon retained index-use lock"),
    );
    let queue = rusqlite::Connection::open(dir.join("requests.db")).unwrap();
    let prior_request: (String, String) = queue
        .query_row(
            "SELECT id,options_json FROM requests WHERE state='done'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    drop(queue);
    fs::write(&index, b"invalid-sqlite-header-with-at-least-20-bytes").unwrap();
    let cleanup = FixtureDaemon::new(&home);
    let read = command(&root, &home, "status")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &marker)
        .output()
        .unwrap();
    if read.status.success() {
        serde_json::from_slice::<Value>(&read.stdout).unwrap();
    } else {
        assert!(
            read.stdout.is_empty(),
            "failed status emitted unverified evidence"
        );
        let reason = String::from_utf8_lossy(&read.stderr);
        assert!(
            reason.contains("recovery_required") || reason.contains("index_not_ready"),
            "{reason}"
        );
    }
    let daemon_pid = fs::read_to_string(&marker).unwrap();
    let recovered = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &marker)
        .arg("--max-file-bytes")
        .arg("3145728")
        .output()
        .unwrap();
    assert!(
        recovered.status.success(),
        "{}",
        String::from_utf8_lossy(&recovered.stderr)
    );
    assert_eq!(fs::read_to_string(&marker).unwrap(), daemon_pid);
    let result: Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(result["publishedRevision"], result["status"]["revision"]);
    // Read activation may publish configured defaults before the accepted custom request.
    assert!(
        result["publishedRevision"]["indexRevision"]
            .as_u64()
            .unwrap()
            >= 1
    );
    let db = rusqlite::Connection::open(&index).unwrap();
    let recorded: String = db
        .query_row(
            "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&recorded).unwrap()["maxFileBytes"],
        3_145_728
    );
    drop(db);
    let queue = rusqlite::Connection::open(dir.join("requests.db")).unwrap();
    let retained: (String, String) = queue
        .query_row(
            "SELECT state,options_json FROM requests WHERE id=?1",
            [&prior_request.0],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(retained, ("done".into(), prior_request.1));
    let custom_done: i64 = queue.query_row(
        "SELECT count(*) FROM requests WHERE state='done' AND json_extract(options_json,'$.maxFileBytes')=3145728", [],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(
        custom_done, 1,
        "accepted custom request must complete durably"
    );
    drop(queue);
    drop(cleanup);
}

#[tokio::test]
async fn corrupted_daemon_startup_uses_configured_options_and_does_not_fallback_when_busy() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use trellis::store::topology::UseGuard;
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let marker = home.join("auto-daemon.pid");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function a() {}\n").unwrap();
    let initial = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &marker)
        .output()
        .unwrap();
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    FixtureDaemon::new(&home).stop_attested().unwrap();
    let index = real_index_db(&home);
    let dir = index.parent().unwrap();
    let use_lock = dir.parent().unwrap().join(format!(
        "{}.lock",
        dir.file_name().unwrap().to_string_lossy()
    ));
    // Socket closure alone does not attest that the old daemon dropped its SH.
    drop(UseGuard::acquire_existing_readonly_exclusive(&use_lock).unwrap());
    let url = "http://127.0.0.1:7331";
    // Open before corruption: Store::open would otherwise attempt recovery.
    let busy_legacy =
        LegacyClient::same_home_with_session(&root, &home, TOKEN, url, 3_145_728, false);
    fs::write(&index, b"broken sqlite index header").unwrap();
    let corrupt = fs::read(&index).unwrap();
    let reader = UseGuard::acquire_existing(&use_lock, false, true).unwrap();
    let busy = busy_legacy
        .get(format!("{url}/api/status"))
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert_eq!(busy.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = busy.json().await.unwrap();
    assert_eq!(body["error"]["code"], "recovery_required");
    assert_eq!(fs::read(&index).unwrap(), corrupt);
    drop(busy_legacy);

    // The CLI sends one durable request to the daemon; it cannot recover this
    // corrupted index while the independent reader holds SH. Do not wait for
    // its reply under SH: an accepted request is not a failed request.
    let stdout_path = temp.path().join("blocked-index.stdout");
    let stderr_path = temp.path().join("blocked-index.stderr");
    let _daemon = FixtureDaemon::new(&home);
    let mut child = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &marker)
        .arg("--max-file-bytes")
        .arg("3145728")
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
        .spawn()
        .unwrap();
    let startup_deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    while !marker.exists() {
        if child.try_wait().unwrap().is_some() || Instant::now() >= startup_deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon-owned CLI exited or did not start while reader held SH");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(child.try_wait().unwrap().is_none());
    assert!(fs::read(&stdout_path).unwrap().is_empty());
    assert_eq!(fs::read(&index).unwrap(), corrupt);
    drop(reader);
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let exit = loop {
        if let Some(exit) = child.try_wait().unwrap() {
            break exit;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon-owned CLI did not complete after SH release");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        exit.success(),
        "{}",
        fs::read_to_string(&stderr_path).unwrap()
    );
    let result: Value = serde_json::from_slice(&fs::read(&stdout_path).unwrap()).unwrap();
    assert_eq!(result["publishedRevision"], result["status"]["revision"]);
    assert_ne!(fs::read(&index).unwrap(), corrupt);

    _daemon.stop_attested().unwrap();
    drop(UseGuard::acquire_existing_readonly_exclusive(&use_lock).unwrap());
    let db = rusqlite::Connection::open(&index).unwrap();
    let recorded: String = db
        .query_row(
            "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&recorded).unwrap()["maxFileBytes"],
        3_145_728
    );
    drop(db);
    let queue = rusqlite::Connection::open(index.with_file_name("requests.db")).unwrap();
    let count: i64 = queue
        .query_row("SELECT count(*) FROM requests", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 2,
        "one initial and one daemon-owned accepted request"
    );
    let (state, options, generation, revision): (String, String, String, i64) = queue.query_row(
        "SELECT state,options_json,result_generation,result_revision FROM requests ORDER BY seq DESC LIMIT 1",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    ).unwrap();
    assert_eq!(state, "done");
    assert_eq!(
        serde_json::from_str::<Value>(&options).unwrap()["maxFileBytes"],
        3_145_728
    );
    assert_eq!(
        generation,
        result["publishedRevision"]["indexGeneration"]
            .as_str()
            .unwrap()
    );
    assert_eq!(
        revision as u64,
        result["publishedRevision"]["indexRevision"]
            .as_u64()
            .unwrap()
    );
}

#[test]
fn cli_round_trip_uses_persistent_store_and_never_executes_workspace() {
    use std::process::{Output, Stdio};
    use std::time::{Duration, Instant};

    // Regular output files make child exit independent of EOF on pipes that a
    // descendant might retain. The deadline also bounds a directly stuck CLI.
    fn bounded_cli(dir: &std::path::Path, label: &str, command: &mut Command) -> Output {
        let stdout_path = dir.join(format!("{label}.stdout"));
        let stderr_path = dir.join(format!("{label}.stderr"));
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&stdout_path).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("{label} CLI did not exit within 20 seconds");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Output {
            status,
            stdout: fs::read(stdout_path).unwrap(),
            stderr: fs::read(stderr_path).unwrap(),
        }
    }

    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let state = temp.path().join("state");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("flow.js"),
        "export function start() { return send(); }\nfunction send() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("package.json"),
        r#"{"scripts":{"postinstall":"touch SHOULD_NOT_EXIST"}}"#,
    )
    .unwrap();
    let before = fs::read(root.join("flow.js")).unwrap();
    let output = bounded_cli(
        temp.path(),
        "index",
        command(&root, &state, "index")
            .arg("--max-file-bytes")
            .arg("1048576"),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"]["stats"]["files"], 1);
    assert_eq!(result["status"]["stats"]["semanticState"], "unavailable");
    let status = bounded_cli(temp.path(), "status", &mut command(&root, &state, "status"));
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["stats"]["files"], 1);
    let output = bounded_cli(
        temp.path(),
        "symbols",
        command(&root, &state, "symbols")
            .arg("--search")
            .arg("start"),
    );
    assert!(output.status.success());
    let symbols: Value = serde_json::from_slice(&output.stdout).unwrap();
    let symbols = &symbols["data"];
    let seed = symbols["items"][0]["id"].as_str().unwrap();
    let output = bounded_cli(
        temp.path(),
        "query",
        command(&root, &state, "query").arg("--seed").arg(seed),
    );
    assert!(output.status.success());
    let view: Value = serde_json::from_slice(&output.stdout).unwrap();
    let view = &view["data"];
    assert_eq!(view["nodes"].as_array().unwrap().len(), 1);
    assert_eq!(view["calls"].as_array().unwrap().len(), 1);
    assert!(view["calls"][0].get("resolution").is_none());
    assert!(view["calls"][0].get("target").is_none());
    let graph = stopped_graph(&root, &state);
    assert_eq!(
        graph["files"][0]["text"],
        std::str::from_utf8(&before).unwrap()
    );
    assert_eq!(fs::read(root.join("flow.js")).unwrap(), before);
    assert!(!root.join("SHOULD_NOT_EXIST").exists());
    assert!(!root.join(".trellis").exists());
}
#[test]
fn cli_refuses_remote_bind_and_invalid_limits() {
    let temp = TempDir::new().unwrap();
    let state = temp.path().join("state");
    let output = command(temp.path(), &state, "serve")
        .arg("--bind")
        .arg("0.0.0.0:8877")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("loopback"));
    let output = command(temp.path(), &state, "index")
        .arg("--max-file-bytes")
        .arg("0")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let output = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
}

#[test]
fn removed_provider_flags_are_unknown_arguments_before_state_creation() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    let home = temp.path().join("home");
    fs::create_dir(&source).unwrap();
    for (case, (flag, value)) in [
        ("--jev-budget-dir", "unused"),
        ("--jev-budget-cents", "500"),
        ("--acp-runner", "unused"),
        ("--acp-state-dir", "unused"),
        ("--acp-max-attempts", "1"),
    ]
    .into_iter()
    .enumerate()
    {
        let output = bounded_invalid_cli(
            temp.path(),
            &format!("removed-provider-{case}"),
            command(&source, &home, "serve").args([flag, value]),
        );
        assert_eq!(output.status.code(), Some(2), "{flag}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("unexpected argument"), "{flag}: {stderr}");
        assert!(stderr.contains(flag), "{flag}: {stderr}");
    }
    assert!(
        !home.exists(),
        "removed flags must fail before state creation"
    );
}

#[test]
fn fixed_locations_and_removed_flag() {
    use std::process::{Output, Stdio};
    use std::time::{Duration, Instant};

    // File-backed output cannot wait forever for a detached descendant to
    // close an inherited pipe. Also bound a CLI that itself fails to exit.
    fn bounded_cli(dir: &std::path::Path, phase: &str, cmd: &mut Command) -> Output {
        let stdout = dir.join(format!("fixed-{phase}.stdout"));
        let stderr = dir.join(format!("fixed-{phase}.stderr"));
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("fixed-locations phase={phase} CLI did not exit within 20 seconds");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        }
    }

    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    let daemon = FixtureDaemon::new(&home);
    let marker = home.join("auto-daemon.pid");
    let cli = |sub| {
        let mut cmd = command(&root, &home, sub);
        cmd.env("TRELLIS_TEST_DAEMON_PID_FILE", &marker);
        cmd
    };
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function seed() {}").unwrap();
    let absent = bounded_cli(temp.path(), "status-absent", &mut cli("status"));
    assert!(
        absent.status.success(),
        "{}",
        String::from_utf8_lossy(&absent.stderr)
    );
    let observed: Value = serde_json::from_slice(&absent.stdout).unwrap();
    assert!(observed["revision"]["indexRevision"].as_u64().unwrap() >= 1);
    let status = bounded_cli(temp.path(), "index", &mut cli("index"));
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let first: Value = serde_json::from_slice(&status.stdout).unwrap();
    let first = &first["status"];
    // The unchanged index completes at the head the status read observed.
    assert_eq!(first["revision"], observed["revision"]);
    let generation = first["revision"]["indexGeneration"].as_str().unwrap();
    assert_eq!(
        uuid::Uuid::parse_str(generation).unwrap().get_version_num(),
        4
    );
    let cache = home.join(if cfg!(target_os = "macos") {
        "Library/Caches/dev.squashmerge.trellis"
    } else {
        ".cache/trellis"
    });
    assert!(
        cache.exists(),
        "fixed cache missing under {}",
        home.display()
    );
    assert!(!home.join("state/cache.db").exists());
    assert!(!home.join("state/workspace.db").exists());
    let mut removed_command = cli("status");
    removed_command
        .arg("--state-dir")
        .arg(home.join("override"));
    let removed = bounded_cli(temp.path(), "removed-state-dir", &mut removed_command);
    assert!(!removed.status.success());
    assert!(String::from_utf8_lossy(&removed.stderr).contains("--state-dir"));
    let final_status: Value = serde_json::from_slice(
        &bounded_cli(temp.path(), "status-final", &mut cli("status")).stdout,
    )
    .unwrap();
    assert_eq!(
        final_status["revision"]["indexGeneration"],
        first["revision"]["indexGeneration"]
    );
    assert_eq!(final_status["revision"], first["revision"]);
    daemon.stop_attested().unwrap();
}
#[test]
fn cli_index_diagnostics_are_opt_in_without_changing_publication() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function seed() {}\n").unwrap();
    let quiet = command(&root, &home, "index")
        .env_remove("TRELLIS_INDEX_DIAGNOSTICS")
        .output()
        .unwrap();
    assert!(
        quiet.status.success(),
        "{}",
        String::from_utf8_lossy(&quiet.stderr)
    );
    let quiet_stderr = String::from_utf8_lossy(&quiet.stderr);
    for marker in ["index-phase ", "index-mode ", "index-writer "] {
        assert!(
            !quiet_stderr.contains(marker),
            "default CLI emitted {marker}"
        );
    }
    assert_eq!(
        serde_json::from_slice::<Value>(&quiet.stdout).unwrap()["publishedRevision"]["indexRevision"],
        1,
        "startup reconciliation publishes r1; the unchanged request completes at it"
    );
    let diagnostic = command(&root, &home, "index")
        .env("TRELLIS_INDEX_DIAGNOSTICS", "1")
        .output()
        .unwrap();
    assert!(
        diagnostic.status.success(),
        "{}",
        String::from_utf8_lossy(&diagnostic.stderr)
    );
    // Index work runs in the detached daemon; the thin CLI has no local
    // capture, publication, or writer timings to print.
    assert!(!String::from_utf8_lossy(&diagnostic.stderr).contains("index-phase "));
    assert_eq!(
        serde_json::from_slice::<Value>(&diagnostic.stdout).unwrap()["publishedRevision"]["indexRevision"]
            .as_u64().unwrap(),
        serde_json::from_slice::<Value>(&quiet.stdout).unwrap()["publishedRevision"]["indexRevision"]
            .as_u64().unwrap(),
        "diagnostics do not change publication: an unchanged request adds no revision"
    );
}

#[test]
fn index_forwards_pair_and_reports_pair() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function seed() {}").unwrap();
    let absent = command(&root, &home, "status").output().unwrap();
    assert!(
        absent.status.success(),
        "{}",
        String::from_utf8_lossy(&absent.stderr)
    );
    let prior: Value = serde_json::from_slice(&absent.stdout).unwrap();
    // A real edit, so the forwarded request publishes past the prior pair.
    fs::write(root.join("a.js"), "function seed() { edited(); }").unwrap();
    let result = command(&root, &home, "index")
        .env("TRELLIS_INDEX_DIAGNOSTICS", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let published: Value = serde_json::from_slice(&result.stdout).unwrap();
    // The detached daemon owns capture and publication diagnostics; the CLI
    // only prints its correlated selected response.
    assert!(!String::from_utf8_lossy(&result.stderr).contains("index-phase "));
    assert_eq!(
        published["status"]["revision"],
        published["publishedRevision"]
    );
    assert_eq!(
        published["status"]["evidenceFormat"],
        "terminal-native-graph-v1"
    );
    assert!(
        published["publishedRevision"]["indexRevision"]
            .as_u64()
            .unwrap()
            > prior["revision"]["indexRevision"].as_u64().unwrap()
    );
    assert_eq!(
        uuid::Uuid::parse_str(
            published["publishedRevision"]["indexGeneration"]
                .as_str()
                .unwrap()
        )
        .unwrap()
        .get_version_num(),
        4
    );
}
#[test]
fn export_is_unknown_command() {
    let temp = TempDir::new().unwrap();
    let result = isolated_command(temp.path())
        .arg("export")
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(result.stdout.is_empty());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("unrecognized subcommand") || error.contains("unknown command"),
        "{error}"
    );
}

#[test]
fn current_commands_pair_matrix() {
    use std::process::{Output, Stdio};
    use std::time::{Duration, Instant};

    // A daemon descendant must not keep Command::output pipes open forever.
    // Fixed phase markers and a per-CLI deadline identify the blocked command.
    fn bounded_cli(dir: &std::path::Path, phase: &str, cmd: &mut Command) -> Output {
        eprintln!("pair-matrix phase=start:{phase}");
        let stdout = dir.join(format!("{phase}.stdout"));
        let stderr = dir.join(format!("{phase}.stderr"));
        let mut child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "pair-matrix phase=timeout:{phase} stderr={}",
                    fs::read_to_string(&stderr).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        eprintln!("pair-matrix phase=done:{phase}");
        let output = Output {
            status,
            stdout: fs::read(stdout).unwrap(),
            stderr: fs::read(stderr).unwrap(),
        };
        assert!(
            output.status.success(),
            "pair-matrix phase=failed:{phase} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function seed() {}").unwrap();
    let index = bounded_cli(temp.path(), "index", &mut command(&root, &home, "index"));
    let pin: Value =
        serde_json::from_slice::<Value>(&index.stdout).unwrap()["publishedRevision"].clone();
    let status: Value = serde_json::from_slice(
        &bounded_cli(
            temp.path(),
            "status-first",
            &mut command(&root, &home, "status"),
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(
        status["revision"]["indexGeneration"],
        pin["indexGeneration"]
    );
    assert_eq!(status["revision"], pin);
    let symbols: Value = serde_json::from_slice(
        &bounded_cli(
            temp.path(),
            "symbols",
            &mut command(&root, &home, "symbols"),
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(
        symbols["data"]["revision"]["indexGeneration"],
        pin["indexGeneration"]
    );
    assert_eq!(symbols["data"]["revision"], pin);
    let seed = symbols["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "seed")
        .unwrap()["id"]
        .as_str()
        .unwrap();
    let query: Value = serde_json::from_slice(
        &bounded_cli(
            temp.path(),
            "query",
            command(&root, &home, "query").arg("--seed").arg(seed),
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(
        query["data"]["revision"]["indexGeneration"],
        pin["indexGeneration"]
    );
    assert_eq!(query["data"]["revision"], pin);
    assert_eq!(
        stopped_graph(&root, &home)["files"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let status: Value = serde_json::from_slice(
        &bounded_cli(
            temp.path(),
            "status-final",
            &mut command(&root, &home, "status"),
        )
        .stdout,
    )
    .unwrap();
    assert_eq!(
        status["revision"]["indexGeneration"],
        pin["indexGeneration"]
    );
    assert_eq!(status["revision"], pin);
}

#[test]
fn external_token_destination_is_rejected_before_git_marker_creation() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::create_dir(root.join(".git")).unwrap();
    let token = root.join("token");
    let serve = bounded_invalid_cli(
        temp.path(),
        "external-serve",
        isolated_command(&home)
            .arg("serve")
            .arg("--workspace")
            .arg(&root)
            .arg("--token-file")
            .arg(&token),
    );
    assert!(!serve.status.success());
    assert!(!root.join(".git/trellis/workspace-id").exists());
    assert!(!token.exists());
}

#[test]
fn overlapping_git_workspace_refuses_before_marker_or_managed_entries() {
    for fixed in ["cache", "data"] {
        for sub in ["status", "index", "symbols", "query", "serve"] {
            let temp = TempDir::new().unwrap();
            let home = temp.path().join("home");
            let (cache, data) = if cfg!(target_os = "macos") {
                (
                    home.join("Library/Caches/dev.squashmerge.trellis"),
                    home.join("Library/Application Support/dev.squashmerge.trellis"),
                )
            } else {
                (
                    home.join(".cache/trellis"),
                    home.join(".local/share/trellis"),
                )
            };
            let workspace = if fixed == "cache" { &cache } else { &data };
            fs::create_dir_all(workspace.join(".git")).unwrap();
            let mut cmd = command(workspace, &home, sub);
            cmd.env("XDG_CACHE_HOME", home.join(".cache"))
                .env("XDG_DATA_HOME", home.join(".local/share"));
            if sub == "query" {
                cmd.arg("--seed").arg("a");
            }
            let output = cmd.output().unwrap();
            assert!(!output.status.success(), "{fixed} {sub}");
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("workspace root overlaps fixed topology"),
                "{fixed} {sub}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                !workspace.join(".git/trellis/workspace-id").exists(),
                "{fixed} {sub}"
            );
            assert!(!cache.join("indexes").exists(), "{fixed} {sub}");
            assert!(!data.join("workspaces").exists(), "{fixed} {sub}");
            assert!(!home.join("token").exists(), "{fixed} {sub}");
        }
    }
}

#[test]
fn gc_report_without_workspace_does_not_create_state() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let output = isolated_command(&home)
        .args(["gc", "--report"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let inventory: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(inventory["derived"], serde_json::json!([]));
    assert!(inventory.get("records").is_none());
    // The user-level daemon may create its private runtime socket, but GC
    // must not materialize any workspace index or saved record.
    assert!(
        !home
            .join("Library/Caches/dev.squashmerge.trellis/indexes")
            .exists()
    );
    assert!(!home.join(".cache/trellis/indexes").exists());
    let denied = isolated_command(&home).arg("gc").output().unwrap();
    assert!(!denied.status.success());
    // The user-level daemon may create its private runtime socket, but GC
    // must not materialize any workspace index or saved record.
    assert!(
        !home
            .join("Library/Caches/dev.squashmerge.trellis/indexes")
            .exists()
    );
    assert!(!home.join(".cache/trellis/indexes").exists());
    let root = temp.path().join("work");
    fs::create_dir(&root).unwrap();
    assert!(
        command(&root, &home, "index")
            .output()
            .unwrap()
            .status
            .success()
    );
    let output = isolated_command(&home)
        .args(["gc", "--report"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let inventory: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(inventory["derived"].as_array().unwrap().len(), 1);
    assert_eq!(inventory["derived"][0]["status"], "busy");
    assert_eq!(inventory["derived"][0]["reason"], "use_lock_busy");
}

#[test]
fn gc_cli_reports_multiple_indexes_in_sorted_order() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let mut workspaces = (0..3)
        .map(|n| {
            let root = temp.path().join(format!("sorted-index-{n}"));
            fs::create_dir(&root).unwrap();
            let identity =
                trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
            (identity.root_key, root)
        })
        .collect::<Vec<_>>();
    workspaces.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, root) in &workspaces {
        let status = command(root, &home, "index").output().unwrap();
        assert!(
            status.status.success(),
            "{}",
            String::from_utf8_lossy(&status.stderr)
        );
    }
    let output = isolated_command(&home)
        .args(["gc", "--report"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let actual = report["derived"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["rootKey"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let mut expected = workspaces
        .into_iter()
        .map(|(key, _)| key)
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(actual, expected);
}

#[test]
fn removed_forget_is_an_unknown_subcommand_without_touching_state() {
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let output = bounded_invalid_cli(
        temp.path(),
        "removed-forget",
        isolated_command(&home).args(["forget", "path-old-record", "--yes"]),
    );
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unrecognized subcommand"), "{stderr}");
    assert!(stderr.contains("forget"), "{stderr}");
    assert!(
        !home.exists(),
        "removed command must fail before state creation"
    );
}

// A test-process Store can prepare the index. Never open it beside the elected daemon.
fn stopped_graph(root: &std::path::Path, home: &std::path::Path) -> Value {
    FixtureDaemon::new(home).stop_attested().unwrap();
    selected_graph(root, home)
}

// Read the selected graph from the real checkout without exercising removed CLI export.
fn selected_graph(root: &std::path::Path, home: &std::path::Path) -> Value {
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let (cache, data) = if cfg!(target_os = "macos") {
        (
            home.join("Library/Caches/dev.squashmerge.trellis"),
            home.join("Library/Application Support/dev.squashmerge.trellis"),
        )
    } else {
        (
            home.join(".cache/trellis"),
            home.join(".local/share/trellis"),
        )
    };
    let roots = TopologyRoots::isolated_for_tests(cache, data);
    let identity = WorkspaceIdentity::discover(Some(root), root).unwrap();
    let store = Store::open(roots, identity).unwrap();
    serde_json::to_value(store.graph().unwrap()).unwrap()
}

// Read the normalized publication, including every native row, from the real daemon's database.
fn real_index_db(home: &std::path::Path) -> std::path::PathBuf {
    fn find(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "index.db") {
                return Some(path);
            }
            if path.is_dir()
                && let Some(db) = find(&path)
            {
                return Some(db);
            }
        }
        None
    }
    find(home).expect("published native database")
}
fn real_native_snapshot(home: &std::path::Path) -> Value {
    use rusqlite::types::ValueRef;
    let db = rusqlite::Connection::open(real_index_db(home)).unwrap();
    let source_set: (String, String) = db
        .query_row("SELECT id,root_id FROM native_source_sets", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    let (generation, revision_number): (String, i64) = db
        .query_row(
            "SELECT index_generation,index_revision FROM index_metadata",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert!(uuid::Uuid::parse_str(&generation).is_ok());
    let pin_id = format!("pin:v1:{generation}:{revision_number}");
    let (header_pin, header_revision): (String, i64) = db
        .query_row(
            "SELECT id,published_index_revision FROM native_revisions WHERE id=?1",
            [&pin_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(header_pin, pin_id);
    assert_eq!(header_revision, revision_number);
    let violations: i64 = db
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    let revision: (String, String, String, String, String) = db.query_row(
        "SELECT id,source_set_id,toolchain_hash,config_hash,dependency_hash FROM native_revisions WHERE id=?1", [&pin_id],
        |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))
    ).unwrap();
    let mut documents = Vec::new();
    let mut stmt = db.prepare(
        "SELECT (SELECT source_set_id FROM native_revisions WHERE id=m.revision_id),v.language,m.path,m.revision_id,v.content_hash,v.byte_length,v.source_bytes FROM revision_documents m JOIN document_versions v ON v.vid=m.vid WHERE m.revision_id=?1 ORDER BY m.path"
    ).unwrap();
    let rows = stmt
        .query_map([&pin_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Vec<u8>>(6)?,
            ))
        })
        .unwrap();
    for row in rows {
        let (set, language, path, revision, hash, length, bytes) = row.unwrap();
        documents.push(serde_json::json!({"sourceSetId":set,"language":language,"path":path,
            "revisionId":revision,"contentHash":hash,"byteLength":length,"bytesHex":hex::encode(bytes)}));
    }
    let new_binding_count: i64 = db
        .query_row(
            "SELECT count(*) FROM revision_producer_bindings WHERE revision_id=?1",
            [&pin_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        new_binding_count, 1,
        "each new published revision has an executable binding"
    );
    let mut all_rows = serde_json::Map::new();
    // Generation-specific binding control is checked separately; compare the
    // actual native/graph/class evidence across independent CLI/daemon roots.
    // Decision 0013's trigram prefilter tables are derived search state.
    let names: Vec<String> = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE '%trigrams%' AND name!='native_binding_epoch' AND name!='native_revision_supersessions' AND (name LIKE 'native_%' OR name LIKE 'head_search_fts%' OR name IN ('document_versions','revision_documents','revision_capture_inputs','graph_projections','graph_nodes','graph_display_labels','class_projections','classes','class_relations')) ORDER BY name").unwrap()
        .query_map([], |r| r.get::<_, String>(0)).unwrap()
        .collect::<rusqlite::Result<_>>().unwrap();
    assert_eq!(
        names.iter().map(String::as_str).collect::<Vec<_>>(),
        [
            "class_projections",
            "class_relations",
            "classes",
            "document_versions",
            "graph_display_labels",
            "graph_nodes",
            "graph_projections",
            "head_search_fts",
            "head_search_fts_config",
            "head_search_fts_content",
            "head_search_fts_data",
            "head_search_fts_docsize",
            "head_search_fts_idx",
            "native_producer_inputs",
            "native_producer_languages",
            "native_producers",
            "native_release_candidate_classes",
            "native_release_candidate_graphs",
            "native_release_candidate_versions",
            "native_revision_release_debt",
            "native_revisions",
            "native_source_set_dependencies",
            "native_source_set_languages",
            "native_source_sets",
            "native_version_ancestor_signature_types",
            "native_version_call_regions",
            "native_version_calls",
            "native_version_capability_families",
            "native_version_capability_gaps",
            "native_version_control_regions",
            "native_version_coverage_roles",
            "native_version_declaration_ancestors",
            "native_version_declarations",
            "native_version_header_items",
            "native_version_headers",
            "native_version_macro_expansions",
            "native_version_own_signature_types",
            "native_version_parameters",
            "native_version_references",
            "native_version_search_cards",
            "revision_capture_inputs",
            "revision_documents",
        ],
        "exact schema-32 native evidence and search table inventory"
    );
    // Compare all corpus cells, not history-dependent FTS shadow segment bytes.
    for name in names
        .into_iter()
        .filter(|name| !name.starts_with("head_search_fts_"))
    {
        let (predicate, alias) = match name.as_str() {
            "native_revisions" => ("id=?1", ""),
            "head_search_fts" => ("?1 IS NOT NULL", ""),
            "native_revision_release_debt"
            | "native_release_candidate_versions"
            | "native_release_candidate_graphs"
            | "native_release_candidate_classes" => ("revision_id=?1", ""),
            "revision_capture_inputs" | "revision_documents" => ("revision_id=?1", ""),
            "native_source_sets" => (
                "id=(SELECT source_set_id FROM native_revisions WHERE id=?1)",
                "",
            ),
            "native_source_set_languages" | "native_source_set_dependencies" => (
                "source_set_id=(SELECT source_set_id FROM native_revisions WHERE id=?1)",
                "",
            ),
            "native_producers" => (
                "EXISTS(SELECT 1 FROM document_versions v JOIN revision_documents d ON d.vid=v.vid WHERE d.revision_id=?1 AND v.producer_id=selected.id AND v.producer_version=selected.version)",
                " selected",
            ),
            "native_producer_languages" | "native_producer_inputs" => (
                "EXISTS(SELECT 1 FROM document_versions v JOIN revision_documents d ON d.vid=v.vid WHERE d.revision_id=?1 AND v.producer_id=selected.producer_id AND v.producer_version=selected.producer_version)",
                " selected",
            ),
            "document_versions" => (
                "vid IN (SELECT vid FROM revision_documents WHERE revision_id=?1)",
                "",
            ),
            "graph_projections" => (
                "gid IN (SELECT gid FROM revision_documents WHERE revision_id=?1)",
                "",
            ),
            "graph_nodes" | "graph_display_labels" => (
                "gid IN (SELECT gid FROM revision_documents WHERE revision_id=?1)",
                "",
            ),
            "class_projections" => (
                "cid IN (SELECT cid FROM revision_documents WHERE revision_id=?1)",
                "",
            ),
            "classes" | "class_relations" => (
                "cid IN (SELECT cid FROM revision_documents WHERE revision_id=?1)",
                "",
            ),
            table if table.starts_with("native_version_") => (
                "vid IN (SELECT vid FROM revision_documents WHERE revision_id=?1)",
                "",
            ),
            _ => panic!("unexpected schema-32 evidence table: {name}"),
        };
        let order = if name == "native_version_references" {
            "vid,owner_syntax_id,ordinal"
        } else if name == "head_search_fts" {
            "vid,syntax_id"
        } else {
            "rowid"
        };
        let mut table = db
            .prepare(&format!(
                "SELECT * FROM \"{name}\"{alias} WHERE {predicate} ORDER BY {order}"
            ))
            .unwrap();
        let columns = table.column_count();
        let records = table
            .query_map([&pin_id], |row| {
                let mut cells = Vec::new();
                for i in 0..columns {
                    let cell = match row.get_ref(i)? {
                        ValueRef::Null => Value::Null,
                        ValueRef::Integer(n) => serde_json::json!(n),
                        ValueRef::Real(n) => serde_json::json!(n),
                        ValueRef::Text(bytes) => serde_json::json!(String::from_utf8_lossy(bytes)),
                        ValueRef::Blob(bytes) => serde_json::json!({"blobHex":hex::encode(bytes)}),
                    };
                    cells.push(cell);
                }
                Ok(cells)
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        if matches!(
            name.as_str(),
            "native_revision_release_debt"
                | "native_release_candidate_versions"
                | "native_release_candidate_graphs"
                | "native_release_candidate_classes"
        ) {
            assert!(
                records.is_empty(),
                "selected HEAD {pin_id} has unexpected v9 release control rows in {name}: {records:?}"
            );
        }
        all_rows.insert(name, serde_json::json!(records));
    }
    let mut expected_corpus: Vec<Value> = all_rows["native_version_search_cards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| serde_json::json!([card[5], card[0], card[1]]))
        .collect();
    let mut actual_corpus = all_rows["head_search_fts"].as_array().unwrap().clone();
    expected_corpus.sort_by_key(Value::to_string);
    actual_corpus.sort_by_key(Value::to_string);
    assert_eq!(
        actual_corpus, expected_corpus,
        "logical head search corpus must contain exactly the selected version cards"
    );
    assert_eq!(
        all_rows.len(),
        37,
        "retain every native evidence table and logical search corpus in schema-32 snapshot"
    );
    serde_json::json!({"sourceSet":{"id":source_set.0,"rootId":source_set.1},
        "revision":{"id":revision.0,"sourceSetId":revision.1,"toolchainHash":revision.2,
            "configHash":revision.3,"dependencyHash":revision.4},
        "documents":documents,"allRows":all_rows})
}

// Only successful same-byte reindex may ignore per-publication pin cells.
// All failed/no-op comparisons must keep real_native_snapshot entirely raw.
fn same_bytes_evidence(snapshot: &Value) -> Value {
    let pin = snapshot["revision"]["id"].as_str().unwrap().to_owned();
    let header = snapshot["allRows"]["native_revisions"].as_array().unwrap();
    assert_eq!(header.len(), 1);
    let header = header[0].as_array().unwrap();
    assert_eq!(header[0], pin);
    assert!(header[5].as_str().is_some_and(|id| !id.is_empty()));
    assert!(header[16].as_i64().is_some_and(|revision| revision > 0));
    for table in ["revision_capture_inputs", "revision_documents"] {
        for row in snapshot["allRows"][table].as_array().unwrap() {
            assert_eq!(row[0], pin, "{table} must reference the admitted pin");
        }
    }
    for doc in snapshot["documents"].as_array().unwrap() {
        assert_eq!(doc["revisionId"], pin);
    }
    let mut evidence = snapshot.clone();
    evidence["revision"]["id"] = serde_json::json!("<current-pin>");
    for doc in evidence["documents"].as_array_mut().unwrap() {
        doc["revisionId"] = serde_json::json!("<current-pin>");
    }
    let header = evidence["allRows"]["native_revisions"]
        .as_array_mut()
        .unwrap()[0]
        .as_array_mut()
        .unwrap();
    header[0] = serde_json::json!("<current-pin>");
    header[16] = serde_json::json!("<current-revision>");
    for table in ["revision_capture_inputs", "revision_documents"] {
        for row in evidence["allRows"][table].as_array_mut().unwrap() {
            row[0] = serde_json::json!("<current-pin>");
        }
    }
    evidence
}

// The recovery fixture uses a direct selected-workspace status router backed
// by the same HOME topology as the executable fixtures above.
#[derive(Clone)]
struct LegacyClient {
    app: axum::Router,
    state: std::sync::Arc<trellis::http::DaemonState>,
}
impl Drop for LegacyClient {
    fn drop(&mut self) {
        self.state.cancel_active();
        self.state.release_checkout_runtime_for_fixture();
    }
}
impl LegacyClient {
    fn same_home_with_session(
        root: &std::path::Path,
        home: &std::path::Path,
        token: &str,
        url: &str,
        max_file_bytes: u64,
        retain_session: bool,
    ) -> Self {
        use trellis::{
            http,
            indexer::IndexOptions,
            store::{
                Store,
                topology::{TopologyRoots, WorkspaceIdentity},
            },
        };
        let (cache, data) = if cfg!(target_os = "macos") {
            (
                home.join("Library/Caches/dev.squashmerge.trellis"),
                home.join("Library/Application Support/dev.squashmerge.trellis"),
            )
        } else {
            (
                home.join(".cache/trellis"),
                home.join(".local/share/trellis"),
            )
        };
        let roots = TopologyRoots::isolated_for_tests(cache, data);
        let identity = WorkspaceIdentity::discover(Some(root), root).unwrap();
        let store = Store::open(roots, identity).unwrap();
        let mut options = IndexOptions::new(root.to_owned());
        options.max_file_bytes = max_file_bytes;
        let address = url.strip_prefix("http://").unwrap().parse().unwrap();
        let state = http::new(store.clone(), options.clone(), token.to_owned(), address).unwrap();
        if retain_session {
            // A daemon-local writer must commit startup reconciliation before selected reads or
            // FIFO claims. The legacy router fixture models only that session.
            let cancel: trellis::model::CancelFlag =
                std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let session = trellis::index_coordinator::establish_serving_session(
                &store,
                Some(&options),
                &cancel,
            )
            .unwrap();
            state.retain_serving_session(session);
        }
        Self {
            app: http::router(state.clone()),
            state,
        }
    }
    fn request(&self, method: reqwest::Method, url: String) -> LegacyRequest {
        LegacyRequest {
            app: self.app.clone(),
            method,
            url,
            token: None,
        }
    }
    fn get(&self, url: String) -> LegacyRequest {
        self.request(reqwest::Method::GET, url)
    }
}
#[derive(Clone)]
struct LegacyRequest {
    app: axum::Router,
    method: reqwest::Method,
    url: String,
    token: Option<String>,
}
impl LegacyRequest {
    fn bearer_auth(mut self, token: &str) -> Self {
        self.token = Some(token.to_owned());
        self
    }
    async fn send(self) -> anyhow::Result<LegacyResponse> {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;
        let url = reqwest::Url::parse(&self.url)?;
        let host = format!(
            "{}:{}",
            url.host_str().unwrap(),
            url.port_or_known_default().unwrap()
        );
        let route = match url.query() {
            Some(q) => format!("{}?{q}", url.path()),
            None => url.path().to_owned(),
        };
        let mut request = Request::builder()
            .method(self.method)
            .uri(route)
            .header("host", host);
        if let Some(token) = self.token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = self.app.oneshot(request.body(Body::empty())?).await?;
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024).await?;
        Ok(LegacyResponse { status, bytes })
    }
}
struct LegacyResponse {
    status: reqwest::StatusCode,
    bytes: axum::body::Bytes,
}
impl LegacyResponse {
    fn status(&self) -> reqwest::StatusCode {
        self.status
    }
    async fn json<T: serde::de::DeserializeOwned>(self) -> serde_json::Result<T> {
        serde_json::from_slice(&self.bytes)
    }
}

async fn startup_status(request: LegacyRequest, context: &str) -> anyhow::Result<Value> {
    let mut attempts = 0;
    let mut last_observed = "no response".to_owned();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            attempts += 1;
            let response = request.clone().send().await.map_err(|error| {
                anyhow::anyhow!(
                    "{context} status attempt {attempts}: {error:#}; last observed {last_observed}"
                )
            })?;
            let status = response.status;
            last_observed = format!(
                "status={status} body={}",
                String::from_utf8_lossy(&response.bytes)
            );
            let body: Value = serde_json::from_slice(&response.bytes).map_err(|error| {
                anyhow::anyhow!(
                    "{context} status attempt {attempts}: {last_observed}; invalid JSON: {error}"
                )
            })?;
            if status == reqwest::StatusCode::OK {
                return Ok(body);
            }
            if status != reqwest::StatusCode::SERVICE_UNAVAILABLE
                || !matches!(
                    body["error"]["code"].as_str(),
                    Some("index_not_ready" | "rebuild_in_progress")
                )
            {
                anyhow::bail!("{context} status attempt {attempts}: {last_observed}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "{context} status timed out after {attempts} attempts; last observed {last_observed}"
        )
    })?
}

fn startup_status_fixture(
    responses: Vec<(reqwest::StatusCode, &'static str)>,
) -> (
    LegacyRequest,
    std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    use std::sync::{Arc, Mutex, atomic::AtomicUsize};
    let responses = Arc::new(Mutex::new(std::collections::VecDeque::from(responses)));
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = attempts.clone();
    let app = axum::Router::new().route(
        "/api/status",
        axum::routing::get(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let response = responses.lock().unwrap().pop_front().unwrap();
            async move { response }
        }),
    );
    (
        LegacyRequest {
            app,
            method: reqwest::Method::GET,
            url: "http://127.0.0.1:7331/api/status".to_owned(),
            token: None,
        },
        attempts,
    )
}

#[tokio::test]
async fn startup_status_retries_only_typed_startup_refusals() {
    use reqwest::StatusCode;
    let (request, attempts) = startup_status_fixture(vec![
        (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":{"code":"index_not_ready","message":"starting"}}"#,
        ),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":{"code":"rebuild_in_progress","message":"rebuilding"}}"#,
        ),
        (
            StatusCode::OK,
            r#"{"catchingUp":true,"revision":{"indexRevision":1}}"#,
        ),
    ]);
    let body = startup_status(request, "initial").await.unwrap();
    assert_eq!(body["catchingUp"], true);
    assert_eq!(body["revision"]["indexRevision"], 1);
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn startup_status_rejects_other_responses_with_the_body() {
    use reqwest::StatusCode;
    for (status, body) in [
        (
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":{"code":"corrupt","message":"broken index"}}"#,
        ),
        (
            StatusCode::CONFLICT,
            r#"{"error":{"code":"index_not_ready"}}"#,
        ),
        (StatusCode::SERVICE_UNAVAILABLE, "not JSON: unavailable"),
    ] {
        let (request, attempts) = startup_status_fixture(vec![(status, body)]);
        let error = startup_status(request, "reopened")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("reopened status attempt 1"), "{error}");
        assert!(error.contains(&status.to_string()), "{error}");
        assert!(error.contains(body), "{error}");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

async fn real_selected_api(
    client: &reqwest::Client,
    url: &str,
    origin: &str,
    token: &str,
    method: reqwest::Method,
    route: &str,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let mut request = client
        .request(method, format!("{url}{route}"))
        .header("Origin", origin)
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

fn blob_id(blob: &Value, prefix: &str) -> String {
    format!(
        "{prefix}{}",
        blob["blobHex"].as_str().expect("stored BLOB ID")
    )
}

fn assert_native_version_witness(native: &Value, version_id: &Value, path: &str, language: &str) {
    let versions: Vec<_> = native["allRows"]["document_versions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row[0] == *version_id)
        .collect();
    assert_eq!(
        versions.len(),
        1,
        "one authenticated document version for {path}"
    );
    assert_eq!(versions[0][2], language);
    assert_eq!(versions[0][3], path);
    let manifests: Vec<_> = native["allRows"]["revision_documents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row[3] == *version_id && row[2] == path)
        .collect();
    assert_eq!(
        manifests.len(),
        1,
        "one admitted revision document for {path}"
    );
    assert_eq!(manifests[0][0], native["revision"]["id"]);
    assert_eq!(manifests[0][1], language);
}

fn assert_fixture_declaration(
    native: &Value,
    symbol: &Value,
    path: &str,
    name: &str,
    kind: &str,
    owner: Option<&str>,
    source: &str,
) {
    let matching: Vec<_> = native["allRows"]["native_version_declarations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| blob_id(&row[1], "sid:v1:") == symbol["id"].as_str().unwrap())
        .collect();
    assert_eq!(matching.len(), 1, "one native declaration for {name}");
    let row = matching[0];
    assert_eq!(symbol["name"], name);
    assert_eq!(symbol["path"], path);
    assert_eq!(symbol["parent"], serde_json::json!(owner));
    let documents: Vec<_> = native["documents"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|doc| doc["path"] == path)
        .collect();
    assert_eq!(documents.len(), 1, "one captured source for {path}");
    let language = documents[0]["language"].as_str().unwrap();
    assert_native_version_witness(native, &row[0], path, language);
    assert_eq!(
        row[2]
            .as_object()
            .map(|_| blob_id(&row[2], "sid:v1:"))
            .as_deref(),
        owner,
        "{name}: native owner"
    );
    assert_eq!(row[3], kind, "{name}: native declaration kind");
    assert_eq!(row[4], "production", "{name}: native role");
    assert_eq!(row[5], name, "{name}: native measured name");
    assert_eq!(
        row[11], symbol["range"]["startByte"],
        "{name}: native start"
    );
    assert_eq!(row[12], symbol["range"]["endByte"], "{name}: native end");
    let start = row[11].as_u64().unwrap() as usize;
    let end = row[12].as_u64().unwrap() as usize;
    assert!(
        source
            .get(start..end)
            .is_some_and(|text| text.contains(name)),
        "{name}: captured bytes must witness declaration"
    );
}

// Exercise the actual executable on both sides of the coordinator, not an in-process router.
#[tokio::test]
async fn real_cli_and_authenticated_daemon_share_native_pair_for_every_language_and_empty_root() {
    use std::time::Duration;
    fn stopped_snapshot(home: &std::path::Path) -> Value {
        let index = real_index_db(home);
        let dir = index.parent().unwrap();
        let use_lock = dir.parent().unwrap().join(format!(
            "{}.lock",
            dir.file_name().unwrap().to_string_lossy()
        ));
        drop(
            trellis::store::topology::UseGuard::acquire_existing_readonly_exclusive(&use_lock)
                .expect("daemon retained index-use lock after stop"),
        );
        real_native_snapshot(home)
    }
    for (name, file, source) in [
        (
            "java",
            "Flow.java",
            "class Flow { void seed() { sink(); } void sink() {} }
",
        ),
        (
            "javascript",
            "flow.js",
            "function seed() { sink(); } function sink() {}
",
        ),
        (
            "python",
            "flow.py",
            "def seed():
    sink()
def sink():
    pass
",
        ),
        (
            "rust",
            "flow.rs",
            "fn seed() { sink(); } fn sink() {}
",
        ),
        ("empty", "", ""),
    ] {
        struct Server(std::process::Child);
        impl Drop for Server {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workspace");
        let home = tmp.path().join("home");
        let daemon = FixtureDaemon::new(&home);
        fs::create_dir(&root).unwrap();
        if !file.is_empty() {
            fs::write(root.join(file), source).unwrap();
        }
        let indexed = command(&root, &home, "index")
            .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
            .output()
            .unwrap();
        assert!(
            indexed.status.success(),
            "{name}: {}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        let first: Value = serde_json::from_slice(&indexed.stdout).unwrap();
        let old_pin = first["publishedRevision"].clone();
        assert_eq!(first["status"]["revision"], old_pin, "{name}");
        daemon.stop_attested().unwrap();
        let graph_before = selected_graph(&root, &home);
        let native_before = stopped_snapshot(&home);
        assert_eq!(
            native_before["documents"].as_array().unwrap().len(),
            usize::from(!file.is_empty()),
            "{name}"
        );
        let post_start_native = native_before.clone();

        // Provision the sole elected daemon; no test-process Store or SQLite
        // handle stays open while it owns the checkout.
        let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = reserved.local_addr().unwrap();
        drop(reserved);
        let origin = format!("http://{address}");
        let root_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .root_key;
        let url = format!("{origin}/api/checkouts/{root_key}");
        let token_file = home.join("token");
        let provision = || {
            Server(
                command(&root, &home, "serve")
                    .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
                    .arg("--bind")
                    .arg(address.to_string())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let mut server = provision();
        let token = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                if let Ok(response) = client.get(format!("{origin}/healthz")).send().await
                    && response.status().is_success()
                    && let Ok(token) = fs::read_to_string(&token_file)
                {
                    break token;
                }
                assert!(
                    server.0.try_wait().unwrap().is_none(),
                    "{name}: serve exited"
                );
                tokio::time::sleep(Duration::from_millis(40)).await;
            }
        })
        .await
        .unwrap();
        let token = token.trim();
        let selected_status =
            |client: &reqwest::Client| client.get(format!("{url}/status")).bearer_auth(token);
        let post_start_status: Value =
            tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
                loop {
                    // The client deadline is shorter than a bounded SQLite busy
                    // wait. Retry only transport timeouts inside the outer startup reconciliation limit;
                    // every answered status still has to prove its selected pin.
                    let response = match selected_status(&client).send().await {
                        Ok(response) => response,
                        Err(error) if error.is_timeout() => {
                            assert!(
                                server.0.try_wait().unwrap().is_none(),
                                "{name}: serve exited during selected status poll"
                            );
                            continue;
                        }
                        Err(error) => panic!("{name}: selected status request: {error}"),
                    };
                    if response.status().is_success() {
                        let status: Value = response.json().await.unwrap();
                        let revision = &status["revision"];
                        assert_eq!(
                            revision["indexGeneration"], old_pin["indexGeneration"],
                            "{name}: unexpected generation during startup reconciliation: {status}"
                        );
                        let current = revision["indexRevision"]
                            .as_u64()
                            .expect("selected index revision");
                        let previous = old_pin["indexRevision"].as_u64().unwrap();
                        let catching_up = status["catchingUp"]
                            .as_bool()
                            .expect("selected catch-up state");
                        assert!(
                            current >= previous,
                            "{name}: selected pin moved backwards: {status}"
                        );
                        // catchingUp clears only once startup reconciliation
                        // committed or confirmed the head (Decision 0009 §2).
                        if !catching_up {
                            break status;
                        }
                    } else {
                        assert_eq!(
                            response.status(),
                            reqwest::StatusCode::SERVICE_UNAVAILABLE,
                            "{name}"
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
        let post_start_pin = post_start_status["revision"].clone();
        assert_eq!(
            post_start_pin["indexGeneration"], old_pin["indexGeneration"],
            "{name}"
        );
        assert_eq!(
            post_start_pin, old_pin,
            "{name}: unchanged startup reconciliation keeps the head (Decision 0006 §3)"
        );
        let follower_status: Value =
            serde_json::from_slice(&command(&root, &home, "status").output().unwrap().stdout)
                .unwrap();
        assert_eq!(follower_status["revision"], post_start_pin, "{name}");
        let follower_symbols = command(&root, &home, "symbols").output().unwrap();
        assert!(follower_symbols.status.success(), "{name}");
        let follower_symbols: Value = serde_json::from_slice(&follower_symbols.stdout).unwrap();
        let follower_symbols = &follower_symbols["data"];
        assert_eq!(follower_symbols["revision"], post_start_pin, "{name}");
        if let Some(follower_seed) = follower_symbols["items"]
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item["id"].as_str())
        {
            let follower_query = command(&root, &home, "query")
                .arg("--seed")
                .arg(follower_seed)
                .output()
                .unwrap();
            assert!(follower_query.status.success(), "{name}");
            let follower_query: Value = serde_json::from_slice(&follower_query.stdout).unwrap();
            assert_eq!(follower_query["data"]["revision"], post_start_pin, "{name}");
        }
        let follower_pin = post_start_pin.clone();
        let request = || {
            client
                .post(format!("{url}/index"))
                .header("Origin", &origin)
                .json(&serde_json::json!({"expectedRevision":follower_pin.clone()}))
        };
        let current_before: Value = client
            .get(format!("{url}/jobs/current"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(current_before["state"], "done", "{name}: initial CLI job");
        let denied = request().bearer_auth("incorrect").send().await.unwrap();
        assert_eq!(denied.status(), 401, "{name}");
        let invalid_origin = client
            .post(format!("{url}/index"))
            .header("Origin", "https://evil.example")
            .bearer_auth(token)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(invalid_origin.status(), 403, "{name}");
        let current: Value = client
            .get(format!("{url}/jobs/current"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            current["id"], current_before["id"],
            "{name}: rejected request added a job"
        );
        assert_eq!(
            current["state"], current_before["state"],
            "{name}: rejected request changed job state"
        );
        assert_eq!(
            current["revision"], current_before["revision"],
            "{name}: rejected request changed job pin"
        );
        // Keep a pinned source read across an unchanged request, which publishes nothing.
        let prior_source_route = if file.is_empty() {
            None
        } else {
            let source_route = format!(
                "/source?path={file}&indexGeneration={}&indexRevision={}",
                post_start_pin["indexGeneration"].as_str().unwrap(),
                post_start_pin["indexRevision"].as_u64().unwrap()
            );
            let (code, source_at) = real_selected_api(
                &client,
                &url,
                &origin,
                token,
                reqwest::Method::GET,
                &source_route,
                None,
            )
            .await;
            assert_eq!(code, 200, "{name}: prior selected source: {source_at}");
            assert_eq!(source_at["revision"], post_start_pin, "{name}");
            assert_eq!(source_at["file"]["text"], source, "{name}");
            Some(source_route)
        };
        let accepted = request().bearer_auth(token).send().await.unwrap();
        assert_eq!(accepted.status(), 202, "{name}");
        let started: Value = accepted.json().await.unwrap();
        let id = started["id"].as_str().unwrap();
        assert_ne!(
            started["id"], current_before["id"],
            "{name}: authorized request needs a new ID"
        );
        let completed = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                let job: Value = client
                    .get(format!("{url}/jobs/{id}"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if !job["finishedAt"].is_null() {
                    break job;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(completed["state"], "done", "{name}: {completed}");
        let pin = completed["revision"].clone();
        assert_eq!(
            pin["indexGeneration"], post_start_pin["indexGeneration"],
            "{name}"
        );
        assert_eq!(
            pin, follower_pin,
            "{name}: the unchanged request completes done at the current pin"
        );
        let status: Value = client
            .get(format!("{url}/status"))
            .bearer_auth(token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(status["revision"], pin, "{name}");
        assert_eq!(
            status["stats"]["files"],
            usize::from(!file.is_empty()),
            "{name}"
        );
        if let Some(source_route) = &prior_source_route {
            let (code, source_at) = real_selected_api(
                &client,
                &url,
                &origin,
                token,
                reqwest::Method::GET,
                source_route,
                None,
            )
            .await;
            assert_eq!(
                code, 200,
                "{name}: retained old selected source: {source_at}"
            );
            assert_eq!(source_at["revision"], post_start_pin, "{name}");
            assert_eq!(source_at["file"]["text"], source, "{name}");
        }
        use sha2::{Digest, Sha256};
        // Collect every selected response against the same committed pin while
        // the sole daemon is live. Inspect native SQLite only after its stop.
        let mut captured = std::collections::HashMap::<String, (reqwest::StatusCode, Value)>::new();
        let fetch = async |method: reqwest::Method, route: String, body: Option<Value>| {
            real_selected_api(&client, &url, &origin, token, method, &route, body).await
        };
        for graph_file in graph_before["files"].as_array().unwrap() {
            let path = graph_file["path"].as_str().unwrap();
            let route = format!(
                "/source?path={path}&indexGeneration={}&indexRevision={}",
                pin["indexGeneration"].as_str().unwrap(),
                pin["indexRevision"].as_u64().unwrap()
            );
            let value = fetch(reqwest::Method::GET, route.clone(), None).await;
            captured.insert(route, value);
        }
        if !file.is_empty() {
            let pinned = format!(
                "indexGeneration={}&indexRevision={}",
                pin["indexGeneration"].as_str().unwrap(),
                pin["indexRevision"].as_u64().unwrap()
            );
            let classes = format!("/classes?{pinned}");
            captured.insert(
                classes.clone(),
                fetch(reqwest::Method::GET, classes, None).await,
            );
            let seed = follower_symbols["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "seed")
                .expect("measured CLI seed")["id"]
                .as_str()
                .unwrap();
            captured.insert(
                "/navigation".into(),
                fetch(
                    reqwest::Method::POST,
                    "/navigation".into(),
                    Some(serde_json::json!({"expectedRevision":pin,"path":file,"line":1})),
                )
                .await,
            );
            captured.insert(
                "/sequence".into(),
                fetch(
                    reqwest::Method::POST,
                    "/sequence".into(),
                    Some(serde_json::json!({"expectedRevision":pin,"seed":seed})),
                )
                .await,
            );
        }
        // No revision was superseded, so CAS on a pin that is not the head.
        let not_head = serde_json::json!({
            "indexGeneration": pin["indexGeneration"],
            "indexRevision": pin["indexRevision"].as_u64().unwrap() + 1,
        });
        let stale = client
            .post(format!("{url}/index"))
            .header("Origin", &origin)
            .json(&serde_json::json!({"expectedRevision":not_head}))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(
            stale.status(),
            202,
            "{name}: stale request durably accepted"
        );
        let stale: Value = stale.json().await.unwrap();
        assert_eq!(
            stale["state"], "queued",
            "{name}: accepted before claim-time CAS"
        );
        let stale_id = stale["id"].as_str().unwrap();
        let failed = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                let row: Value = client
                    .get(format!("{url}/jobs/{stale_id}"))
                    .bearer_auth(token)
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if !row["finishedAt"].is_null() {
                    break row;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(failed["state"], "failed", "{name}: {failed}");
        assert_eq!(failed["error"]["code"], "revision_conflict", "{name}");
        let unchanged_status: Value = selected_status(&client)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            unchanged_status["revision"], pin,
            "{name}: stale request changed committed head"
        );
        daemon.stop_attested().unwrap();
        drop(server);
        let graph_after = selected_graph(&root, &home);
        let native_after = stopped_snapshot(&home);
        let before_header = &native_before["allRows"]["native_revisions"][0];
        let after_header = &native_after["allRows"]["native_revisions"][0];
        assert_eq!(
            before_header, after_header,
            "{name}: unchanged startup and request publish no revision"
        );
        assert_eq!(after_header[16], pin["indexRevision"], "{name}");
        assert_eq!(
            after_header[0],
            format!(
                "pin:v1:{}:{}",
                pin["indexGeneration"].as_str().unwrap(),
                pin["indexRevision"].as_u64().unwrap()
            ),
            "{name}: header pin must match public status"
        );
        assert_eq!(
            same_bytes_evidence(&native_after),
            same_bytes_evidence(&post_start_native),
            "{name}: all 36 native schema-32 evidence tables, logical head search corpus and documents must match after only publication identity normalization"
        );
        assert_eq!(
            graph_after, graph_before,
            "{name}: identical bytes must reproduce graph"
        );
        let root_id =
            trellis::store::topology::WorkspaceIdentity::discover_unattached(Some(&root), &root)
                .unwrap()
                .record_id;
        assert_eq!(native_after["sourceSet"]["rootId"], root_id, "{name}");
        assert_eq!(
            native_after["revision"]["sourceSetId"], native_after["sourceSet"]["id"],
            "{name}"
        );
        for field in ["toolchainHash", "configHash", "dependencyHash"] {
            assert_eq!(
                native_after["revision"][field].as_str().unwrap().len(),
                64,
                "{name}: {field}"
            );
        }
        let documents = native_after["documents"].as_array().unwrap();
        let graph_files = graph_after["files"].as_array().unwrap();
        assert_eq!(
            documents.len(),
            graph_files.len(),
            "{name}: every graph source needs native bytes"
        );
        for doc in documents {
            let path = doc["path"].as_str().unwrap();
            let graph_file = graph_files.iter().find(|row| row["path"] == path).unwrap();
            let bytes = graph_file["text"].as_str().unwrap().as_bytes();
            let digest = hex::encode(Sha256::digest(bytes));
            assert_eq!(
                doc["sourceSetId"], native_after["sourceSet"]["id"],
                "{name}"
            );
            assert_eq!(doc["revisionId"], native_after["revision"]["id"], "{name}");
            assert_eq!(doc["language"], graph_file["language"], "{name}");
            assert_eq!(doc["contentHash"], digest, "{name}");
            assert_eq!(doc["contentHash"], graph_file["hash"], "{name}");
            assert_eq!(doc["byteLength"], bytes.len(), "{name}");
            assert_eq!(doc["bytesHex"], hex::encode(bytes), "{name}");
            let route = format!(
                "/source?path={path}&indexGeneration={}&indexRevision={}",
                pin["indexGeneration"].as_str().unwrap(),
                pin["indexRevision"].as_u64().unwrap()
            );

            let (code, source_at) = captured
                .get(&route)
                .expect("selected HTTP response")
                .clone();
            assert_eq!(code, 200, "{name}: {source_at}");
            assert_eq!(source_at["revision"], pin, "{name}");
            assert_eq!(
                source_at["file"], *graph_file,
                "{name}: pinned source equals graph and native bytes"
            );
        }
        if !file.is_empty() {
            assert_eq!(documents[0]["path"], file, "{name}");
            assert_eq!(graph_files[0]["text"], source, "{name}");
            let pinned = format!(
                "indexGeneration={}&indexRevision={}",
                pin["indexGeneration"].as_str().unwrap(),
                pin["indexRevision"].as_u64().unwrap()
            );
            let measured = graph_after["nodes"].as_array().unwrap();

            let (code, classes) = captured
                .get(&format!("/classes?{pinned}"))
                .expect("selected HTTP response")
                .clone();
            assert_eq!(code, 200, "{name}: {classes}");
            assert_eq!(classes["revision"], pin, "{name}");
            let measured_classes: Vec<_> = measured
                .iter()
                .filter(|node| node["kind"] == "class")
                .collect();
            assert_eq!(
                measured_classes.len(),
                usize::from(name == "java"),
                "{name}: expected independently measured class set"
            );
            let class_owner = if name == "java" {
                Some(measured_classes[0]["id"].as_str().unwrap())
            } else {
                None
            };
            let returned_classes = classes["items"].as_array().unwrap();
            assert_eq!(
                returned_classes.len(),
                measured_classes.len(),
                "{name}: truthful class catalog"
            );
            for class in returned_classes {
                let symbol = &class["symbol"];
                assert!(
                    measured_classes.contains(&symbol),
                    "{name}: class must be a captured declaration: {class}"
                );
                assert_eq!(
                    symbol["name"], "Flow",
                    "{name}: only Java has a measured class"
                );
                assert_fixture_declaration(
                    &native_after,
                    symbol,
                    file,
                    "Flow",
                    "type",
                    None,
                    source,
                );
            }
            assert_eq!(
                returned_classes.len(),
                usize::from(name == "java"),
                "{name}"
            );

            assert_eq!(follower_symbols["revision"], pin, "{name}");
            let seed = follower_symbols["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "seed")
                .expect("measured CLI seed declaration")["id"]
                .as_str()
                .unwrap();
            let measured_seed = measured.iter().find(|node| node["id"] == seed).unwrap();
            assert_eq!(measured_seed["path"], file, "{name}");
            let callable_kind = if name == "java" { "method" } else { "function" };
            assert_fixture_declaration(
                &native_after,
                measured_seed,
                file,
                "seed",
                callable_kind,
                class_owner,
                source,
            );

            let (code, navigation) = captured
                .get("/navigation")
                .expect("selected HTTP response")
                .clone();
            assert_eq!(code, 200, "{name}: {navigation}");
            assert_eq!(navigation["revision"], pin, "{name}");
            let targets = navigation["targets"].as_array().unwrap();
            assert!(
                targets.iter().any(|target| target["symbol"]["id"] == seed),
                "{name}: source line must navigate to measured seed: {navigation}"
            );
            for target in targets {
                let symbol = &target["symbol"];
                assert_eq!(symbol["path"], file, "{name}");
                assert_eq!(target["matchKind"], "measured", "{name}");
                assert!(
                    measured.iter().any(|node| node == symbol),
                    "{name}: navigation only to captured symbol: {target}"
                );
                let target_name = symbol["name"].as_str().unwrap();
                match target_name {
                    "Flow" if name == "java" => assert_fixture_declaration(
                        &native_after,
                        symbol,
                        file,
                        "Flow",
                        "type",
                        None,
                        source,
                    ),
                    "seed" | "sink" => assert_fixture_declaration(
                        &native_after,
                        symbol,
                        file,
                        target_name,
                        callable_kind,
                        class_owner,
                        source,
                    ),
                    _ => panic!("{name}: unexpected navigation declaration {target}"),
                }
            }

            let (code, sequence) = captured
                .get("/sequence")
                .expect("selected HTTP response")
                .clone();
            assert_eq!(code, 200, "{name}: {sequence}");
            assert_eq!(sequence["revision"], pin, "{name}");
            assert_eq!(
                sequence["seed"], *measured_seed,
                "{name}: sequence selected wrong seed"
            );
            let graph_calls = graph_after["calls"].as_array().unwrap();
            let native_calls = native_after["allRows"]["native_version_calls"]
                .as_array()
                .unwrap();
            assert_eq!(
                graph_calls.len(),
                1,
                "{name}: fixture has exactly one measured sink call"
            );
            assert_eq!(
                native_calls.len(),
                1,
                "{name}: exactly one native sink call"
            );
            let call = &graph_calls[0];
            let native_call = &native_calls[0];
            let expected_start = source.find("sink()").unwrap();
            let expected_end = expected_start + "sink()".len();
            assert_eq!(source.get(expected_start..expected_end), Some("sink()"));
            assert_eq!(call["caller"], seed, "{name}: measured call owner");
            assert_eq!(call["path"], file, "{name}: measured call path");
            assert_eq!(call["calleeText"], "sink", "{name}: measured call spelling");
            assert_eq!(call["range"]["startByte"], expected_start, "{name}");
            assert_eq!(call["range"]["endByte"], expected_end, "{name}");
            assert_native_version_witness(&native_after, &native_call[0], file, name);
            assert_eq!(
                blob_id(&native_call[1], "occ:v2:"),
                call["id"].as_str().unwrap(),
                "{name}: same measured native call ID"
            );
            assert_eq!(
                blob_id(&native_call[2], "sid:v1:"),
                seed,
                "{name}: native call owner"
            );
            assert_eq!(native_call[4], expected_start, "{name}: native start byte");
            assert_eq!(native_call[5], expected_end, "{name}: native end byte");
            assert_eq!(native_call[8], "sink", "{name}: native callee spelling");
            fn measured_steps(
                steps: &[Value],
                file: &str,
                text: &str,
                calls: &[Value],
                native: &[Value],
                seen: &mut Vec<String>,
                flattened: &mut Vec<Value>,
            ) {
                for step in steps {
                    flattened.push(step.clone());
                    assert_eq!(step["path"], file, "sequence step path: {step}");
                    let start = step["range"]["startByte"].as_u64().unwrap() as usize;
                    let end = step["range"]["endByte"].as_u64().unwrap() as usize;
                    assert!(
                        start <= end && end <= text.len(),
                        "step outside captured source: {step}"
                    );
                    assert!(
                        step.get("target").is_none() && step.get("resolution").is_none(),
                        "syntax must never infer dispatch: {step}"
                    );
                    if let Some(id) = step["callId"].as_str() {
                        let call = calls
                            .iter()
                            .find(|call| call["id"] == id)
                            .expect("measured graph call");
                        assert_eq!(step["path"], call["path"]);
                        assert_eq!(step["range"], call["range"]);
                        assert!(
                            native.iter().any(|row| blob_id(&row[1], "occ:v2:") == id),
                            "native call ID absent: {id}"
                        );
                        seen.push(id.to_owned());
                    }
                    for branch in ["children", "alternate"] {
                        measured_steps(
                            step[branch].as_array().unwrap(),
                            file,
                            text,
                            calls,
                            native,
                            seen,
                            flattened,
                        );
                    }
                }
            }
            let mut seen = Vec::new();
            let mut flattened = Vec::new();
            measured_steps(
                sequence["steps"].as_array().unwrap(),
                file,
                source,
                graph_calls,
                native_calls,
                &mut seen,
                &mut flattened,
            );
            assert_eq!(
                seen,
                vec![call["id"].as_str().unwrap().to_owned()],
                "{name}: exactly one source-witnessed sink call and no duplicate/extra call IDs"
            );
            assert_eq!(
                flattened.len(),
                1,
                "{name}: no invented null-call/control steps: {sequence}"
            );
            let step = &flattened[0];
            assert_eq!(step["kind"], "call", "{name}");
            assert_eq!(step["callId"], call["id"], "{name}");
            assert_eq!(step["label"], "sink", "{name}");
            assert_eq!(step["path"], file, "{name}");
            assert_eq!(step["range"], call["range"], "{name}");
            assert_eq!(source.get(expected_start..expected_end), Some("sink()"));
            let expected_seed: trellis::model::Symbol =
                serde_json::from_value(measured_seed.clone()).unwrap();
            let expected_file: trellis::model::SourceFile =
                serde_json::from_value(graph_files[0].clone()).unwrap();
            let expected_calls: Vec<trellis::model::CallSite> = graph_calls
                .iter()
                .map(|call| serde_json::from_value(call.clone()).unwrap())
                .collect();
            let expected_pin: trellis::model::IndexPin =
                serde_json::from_value(pin.clone()).unwrap();
            let mut expected_sequence = trellis::behavior::build_sequence(
                expected_pin,
                &expected_seed,
                &expected_file,
                &expected_calls,
                false,
            )
            .unwrap();
            // No producer is published: the store says why it shows syntax (#522).
            expected_sequence.semantic = trellis::behavior::SequenceSemantic::syntax(Some(
                trellis::graph_query::NO_PRODUCER_NOTE.into(),
            ));
            assert_eq!(
                sequence,
                serde_json::to_value(&expected_sequence).unwrap(),
                "{name}: complete sequence wire matches the captured source projection"
            );
        }
    }
}

#[test]
fn virgin_cli_activation_contention_expires_before_enqueue_and_new_index_succeeds() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    use trellis::store::{
        Store,
        topology::{TopologyRoots, UseGuard, WorkspaceIdentity},
    };

    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("flow.js"), "function start() {}\n").unwrap();
    let _daemon = FixtureDaemon::new(&home);
    let (cache, data) = if cfg!(target_os = "macos") {
        (
            home.join("Library/Caches/dev.squashmerge.trellis"),
            home.join("Library/Application Support/dev.squashmerge.trellis"),
        )
    } else {
        (
            home.join(".cache/trellis"),
            home.join(".local/share/trellis"),
        )
    };
    let roots = TopologyRoots::isolated_for_tests(cache, data);
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let use_lock = roots.index_use_lock(&identity);
    let store = Store::open(roots, identity).unwrap();
    assert_eq!(store.index_baseline().unwrap().index_revision, 0);
    let queue_path = store.request_db_path();
    assert!(
        !queue_path.exists(),
        "fresh Ready index must have a virgin queue"
    );
    // Store must release every SQLite handle before another process contends on
    // the index-use lock. SH blocks the virgin queue's mandatory EX publish.
    drop(store);
    let reader = UseGuard::acquire_existing_readonly(&use_lock).unwrap();
    let stdout_path = temp.path().join("contended-index.stdout");
    let stderr_path = temp.path().join("contended-index.stderr");
    let marker = home.join("auto-daemon.pid");
    let mut child = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &marker)
        .stdout(Stdio::from(fs::File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("contended CLI index did not exit after its 5s read wait");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success(), "contended index unexpectedly succeeded");
    assert!(
        fs::read(&stdout_path).unwrap().is_empty(),
        "failed index emitted an accepted ID"
    );
    let stderr = fs::read_to_string(&stderr_path).unwrap();
    assert!(
        stderr.contains("storage_busy: CLI read contention wait expired"),
        "unexpected pre-acceptance error: {stderr}"
    );
    assert!(
        !queue_path.exists(),
        "a failed pre-acceptance index created requests.db"
    );
    drop(reader);

    // This is a new CLI invocation, not a replay of an uncertain enqueue.
    let fresh = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &marker)
        .output()
        .unwrap();
    assert!(
        fresh.status.success(),
        "{}",
        String::from_utf8_lossy(&fresh.stderr)
    );
    let result: Value = serde_json::from_slice(&fresh.stdout).unwrap();
    assert_eq!(result["publishedRevision"], result["status"]["revision"]);
    // Startup reconciliation publishes r1; the unchanged claimed request completes at it.
    assert_eq!(result["publishedRevision"]["indexRevision"], 1);

    // Raw SQLite reads may close descriptors on the live inode and drop its
    // process locks. Attest daemon shutdown and released use lock first.
    FixtureDaemon::new(&home).stop_attested().unwrap();
    let _exclusive = UseGuard::acquire_existing_readonly_exclusive(&use_lock)
        .expect("retired daemon retained the index-use lock");
    let db = rusqlite::Connection::open_with_flags(
        &queue_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (count, distinct_ids, state, revision): (i64, i64, String, i64) = db
        .query_row(
            "SELECT count(*), count(DISTINCT id), min(state), min(result_revision) FROM requests",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (count, distinct_ids, state.as_str(), revision),
        (1, 1, "done", 1)
    );
}

#[test]
fn daemon_publication_is_independent_of_cli_output_file() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("flow.js"), "function start() {}\n").unwrap();
    let output_path = temp.path().join("index.stdout");
    let stderr_path = temp.path().join("index.stderr");
    let daemon_pid = temp.path().join("daemon.pid");
    struct ElectedDaemon(std::path::PathBuf);
    impl Drop for ElectedDaemon {
        fn drop(&mut self) {
            if let Ok(pid) = fs::read_to_string(&self.0)
                && let Ok(pid) = pid.parse::<u32>()
                && pid <= i32::MAX as u32
            {
                // The elected daemon writes this marker under the private fixture.
                unsafe { libc::kill(pid as i32, libc::SIGTERM) };
            }
        }
    }
    let _daemon = ElectedDaemon(daemon_pid.clone());
    let mut child = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &daemon_pid)
        .stdout(Stdio::from(fs::File::create(&output_path).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if let Some(exit) = child.try_wait().unwrap() {
            assert!(
                exit.success(),
                "{}",
                fs::read_to_string(&stderr_path).unwrap()
            );
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon-owned CLI index did not finish within 15s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let result: Value = serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
    assert_eq!(result["publishedRevision"], result["status"]["revision"]);
    assert_eq!(result["status"]["stats"]["files"], 1);
    let pid: u32 = fs::read_to_string(daemon_pid).unwrap().parse().unwrap();
    assert_ne!(pid, child.id(), "index CLI must not be the elected daemon");
    let queue = real_index_db(&home).with_file_name("requests.db");
    let db =
        rusqlite::Connection::open_with_flags(queue, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let (count, done): (i64, i64) = db
        .query_row(
            "SELECT count(*),sum(CASE WHEN state='done' THEN 1 ELSE 0 END) FROM requests",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((count, done), (1, 1), "one daemon-owned durable request");
}

#[test]
fn explicit_cli_unchanged_claim_acks_done_without_new_revision() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("a.js"), "function unchanged() {}\n").unwrap();
    // Startup reconciliation publishes r1; each unchanged claim completes at it.
    for expected in [1_i64, 1] {
        let output = command(&root, &home, "index").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let db = rusqlite::Connection::open(real_index_db(&home)).unwrap();
        let actual: i64 = db
            .query_row("SELECT index_revision FROM index_metadata", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            actual, expected,
            "an unchanged claimed request publishes no revision"
        );
        let immutable: i64 = db
            .query_row("SELECT count(*) FROM document_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            immutable, 1,
            "unchanged CLI claim must reuse measured document facts"
        );
        let count: i64 = db
            .query_row("SELECT count(*) FROM revision_documents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "an unchanged claim adds no manifest");
        let queue =
            rusqlite::Connection::open(real_index_db(&home).with_file_name("requests.db")).unwrap();
        let (state, revision): (String, i64) = queue
            .query_row(
                "SELECT state, result_revision FROM requests ORDER BY seq DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((state.as_str(), revision), ("done", expected));
    }
}

#[test]
fn architecture_cli_pins_layer_deviations_references_and_exact_exceptions_without_enqueue() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let daemon = FixtureDaemon::new(&home);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir(root.join(".trellis")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod high; mod low;\n").unwrap();
    fs::write(root.join("src/high.rs"), "pub fn target() {}\n").unwrap();
    fs::write(
        root.join("src/low.rs"),
        r#"pub fn run() { crate::high::target(); }
#[cfg(test)]
mod tests {
    #[test]
    fn check() { crate::high::target(); }
}
#[cfg(feature = "test-support")]
pub fn fixture() { crate::high::target(); }
"#,
    )
    .unwrap();
    let config = r#"version = 1
[layers.high]
order = 1
paths = ["src/high.rs"]
[layers.low]
order = 2
paths = ["src/low.rs"]
"#;
    fs::write(root.join(".trellis/rules.toml"), config).unwrap();
    let indexed = command(&root, &home, "index").output().unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let indexed: Value = serde_json::from_slice(&indexed.stdout).unwrap();
    let basis = &indexed["publishedRevision"];
    let pin = format!(
        "{}:{}",
        basis["indexGeneration"].as_str().unwrap(),
        basis["indexRevision"].as_u64().unwrap()
    );
    let checked = isolated_command(&home)
        .args(["rules", "check", "--format", "json", "--workspace"])
        .arg(&root)
        .args(["--pin", &pin, "--path", "src/low.rs"])
        .output()
        .unwrap();
    assert!(!checked.status.success(), "upward references must fail");
    let checked: Value = serde_json::from_slice(&checked.stdout).unwrap();
    let payload = &checked["payload"];
    assert_eq!(payload["indexGeneration"], basis["indexGeneration"]);
    assert_eq!(payload["indexRevision"], basis["indexRevision"]);
    assert_eq!(payload["counts"]["deviates"], 1);
    assert_eq!(payload["instances"].as_array().unwrap().len(), 1);
    let violation = &payload["instances"][0];
    assert_eq!(violation["rule"], "$layers");
    assert_eq!(violation["status"], "deviates");
    assert_eq!(violation["excepted"], false);
    assert_eq!(violation["target"]["path"], "src/low.rs");
    assert_eq!(violation["destination"]["path"], "src/high.rs");
    assert_eq!(violation["referenceCount"], 2);
    assert_eq!(payload["testOnly"].as_array().unwrap().len(), 1);
    assert_eq!(payload["testOnly"][0]["referenceCount"], 1);
    assert_eq!(payload["rulesFile"]["path"], ".trellis/rules.toml");
    assert_eq!(payload["rulesFile"]["source"], "checkout-config");

    let references = command(&root, &home, "references")
        .args(["--pin", &pin, "--path", "src/low.rs", "--limit", "1"])
        .output()
        .unwrap();
    assert!(
        references.status.success(),
        "{}",
        String::from_utf8_lossy(&references.stderr)
    );
    let references: Value = serde_json::from_slice(&references.stdout).unwrap();
    let references = &references["payload"];
    assert_eq!(references["indexGeneration"], basis["indexGeneration"]);
    assert_eq!(references["indexRevision"], basis["indexRevision"]);
    assert_eq!(references["evidenceTier"], "syntax");
    assert_eq!(references["semanticComplete"], false);
    assert_eq!(references["counts"]["mappedSites"], 3);
    assert_eq!(references["counts"]["unresolvedSites"], 0);
    assert_eq!(references["production"].as_array().unwrap().len(), 1);
    assert_eq!(references["tests"].as_array().unwrap().len(), 1);
    let production = &references["production"][0];
    assert_eq!(production["source"], violation["target"]);
    assert_eq!(production["target"], violation["destination"]);
    assert_eq!(production["count"], 2, "test-support remains production");
    assert_eq!(production["sites"].as_array().unwrap().len(), 1);
    assert_eq!(production["sites"][0]["role"], "production");
    assert_eq!(production["omittedSites"], 1);
    let tests = &references["tests"][0];
    assert_eq!(tests["source"], production["source"]);
    assert_eq!(tests["target"], production["target"]);
    assert_eq!(tests["count"], 1);
    assert_eq!(tests["sites"][0]["role"], "test");
    assert_eq!(tests["omittedSites"], 0);

    let target = &violation["target"];
    let destination = &violation["destination"];
    let exception = r#"
[[exceptions]]
rule = "$layers"
target = { language = "rust", path = "src/low.rs" }
destination = { language = "rust", path = "src/high.rs" }
reason = "Reviewed upward adapter"
"#;
    fs::write(
        root.join(".trellis/rules.toml"),
        format!("{config}{exception}"),
    )
    .unwrap();
    let excepted = isolated_command(&home)
        .args(["rules", "check", "--format", "json", "--workspace"])
        .arg(&root)
        .args(["--pin", &pin, "--path", "src/low.rs"])
        .output()
        .unwrap();
    assert!(
        excepted.status.success(),
        "{}",
        String::from_utf8_lossy(&excepted.stderr)
    );
    let excepted: Value = serde_json::from_slice(&excepted.stdout).unwrap();
    let excepted = &excepted["payload"];
    assert_eq!(excepted["indexGeneration"], basis["indexGeneration"]);
    assert_eq!(excepted["indexRevision"], basis["indexRevision"]);
    assert_eq!(excepted["counts"]["deviates"], 0);
    assert_eq!(excepted["counts"]["excepted"], 1);
    assert_eq!(excepted["instances"][0]["status"], "deviates");
    assert_eq!(excepted["instances"][0]["excepted"], true);
    assert_eq!(excepted["instances"][0]["target"], *target);
    assert_eq!(excepted["instances"][0]["destination"], *destination);
    assert_eq!(
        excepted["instances"][0]["exceptionReason"],
        "Reviewed upward adapter"
    );
    assert_eq!(excepted["instances"][0]["evidence"], violation["evidence"]);
    assert_ne!(
        excepted["rulesFile"]["contentHash"], payload["rulesFile"]["contentHash"],
        "current configuration is identified separately from the code pin"
    );
    let status = command(&root, &home, "status").output().unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(
        status["revision"], *basis,
        "reads must not publish a revision"
    );

    // Inspect the queue only after the sole writer has retired its SQLite handles.
    daemon.stop_attested().unwrap();
    let dbpath = real_index_db(&home);
    let use_lock = dbpath.parent().unwrap().with_extension("lock");
    let _exclusive =
        trellis::store::topology::UseGuard::acquire_existing_readonly_exclusive(&use_lock)
            .expect("retired daemon retained the index-use lock");
    let queue = rusqlite::Connection::open_with_flags(
        dbpath.with_file_name("requests.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let (requests, done): (i64, i64) = queue
        .query_row(
            "SELECT count(*), sum(CASE WHEN state='done' THEN 1 ELSE 0 END) FROM requests",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((requests, done), (1, 1), "only index may enqueue work");
}

#[test]
fn architecture_cli_unknown_call_rule_succeeds_without_invented_bindings() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let _daemon = FixtureDaemon::new(&home);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir(root.join(".trellis")).unwrap();
    fs::write(
        root.join("src/flow.rs"),
        "fn target() {}\nfn run() { target(); }\n",
    )
    .unwrap();
    fs::write(
        root.join(".trellis/rules.toml"),
        r#"version = 1
[roles.source]
select = { name = "run", language = ["rust"] }
[roles.destination]
select = { name = "target" }
[[rules]]
id = "needs-target"
for_each = "source"
require = [{ calls = "destination" }]
"#,
    )
    .unwrap();
    let indexed = command(&root, &home, "index").output().unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let indexed: Value = serde_json::from_slice(&indexed.stdout).unwrap();
    let checked = isolated_command(&home)
        .args(["rules", "check", "--format", "json", "--workspace"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "unknown-only evaluation must succeed: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
    let checked: Value = serde_json::from_slice(&checked.stdout).unwrap();
    let payload = &checked["payload"];
    assert_eq!(
        payload["indexGeneration"],
        indexed["publishedRevision"]["indexGeneration"]
    );
    assert_eq!(
        payload["indexRevision"],
        indexed["publishedRevision"]["indexRevision"]
    );
    assert_eq!(payload["counts"]["deviates"], 0);
    assert_eq!(payload["counts"]["unknown"], 1);
    assert_eq!(payload["instances"].as_array().unwrap().len(), 1);
    let instance = &payload["instances"][0];
    assert_eq!(instance["rule"], "needs-target");
    assert_eq!(instance["status"], "unknown");
    assert_eq!(instance["constraints"][0]["kind"], "calls");
    assert_eq!(instance["constraints"][0]["status"], "unknown");
    assert!(
        instance["constraints"][0]["unresolved"]
            .as_array()
            .unwrap()
            .contains(&Value::String("call_binding_unavailable".into()))
    );
    assert!(
        instance["constraints"][0]["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|evidence| evidence["kind"] == "call" && evidence["callId"].is_string())
    );
}

#[test]
fn architecture_cli_invalid_rules_is_a_located_json_failure_not_a_verdict() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let _daemon = FixtureDaemon::new(&home);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir(root.join(".trellis")).unwrap();
    fs::write(root.join("src/flow.rs"), "fn run() {}\n").unwrap();
    let config = r#"version = 1
[roles.source]
select = { name = "run" }
[[rules]]
id = "bad-role"
for_each = "missing"
require = [{ calls = "source" }]
"#;
    fs::write(root.join(".trellis/rules.toml"), config).unwrap();
    let indexed = command(&root, &home, "index").output().unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let checked = isolated_command(&home)
        .args(["rules", "check", "--format", "json", "--workspace"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(!checked.status.success(), "invalid config must fail");
    let checked: Value = serde_json::from_slice(&checked.stdout).unwrap();
    assert!(
        checked.get("payload").is_none(),
        "no partial verdict: {checked}"
    );
    let error = &checked["error"];
    assert_eq!(error["code"], "invalid_rules");
    assert_eq!(error["kind"], "reference");
    assert_eq!(error["path"], ".trellis/rules.toml");
    assert_eq!(error["attribution"], "token");
    assert_eq!(error["line"], 6);
    assert!(error["column"].as_u64().unwrap() > 0);
    let start = usize::try_from(error["range"]["start"].as_u64().unwrap()).unwrap();
    let end = usize::try_from(error["range"]["end"].as_u64().unwrap()).unwrap();
    assert!(start < end && end <= config.len());
    assert!(config[start..end].contains("missing"));
}

#[tokio::test]
async fn architecture_cli_waits_for_startup_reconciliation_after_typed_admission_refusal() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use trellis::store::topology::UseGuard;

    // Keep Unix socket paths below the platform sockaddr_un bound.
    let temp = tempfile::Builder::new()
        .prefix("trellis-cli-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let daemon = FixtureDaemon::new(&home);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod high; mod low;\n").unwrap();
    fs::write(root.join("src/high.rs"), "pub fn target() {}\n").unwrap();
    fs::write(
        root.join("src/low.rs"),
        "pub fn run() { crate::high::target(); }\n",
    )
    .unwrap();
    let indexed = command(&root, &home, "index").output().unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let indexed: Value = serde_json::from_slice(&indexed.stdout).unwrap();
    daemon.stop_attested().unwrap();
    let dbpath = real_index_db(&home);
    let use_lock = dbpath.parent().unwrap().with_extension("lock");
    {
        let _exclusive = UseGuard::acquire_existing_readonly_exclusive(&use_lock)
            .expect("retired daemon retained the index-use lock");
        let db = rusqlite::Connection::open(&dbpath).unwrap();
        // An obsolete disposable index requires normal startup recreation.
        db.execute_batch(
            "PRAGMA ignore_check_constraints=ON;
             UPDATE index_metadata SET schema_version=7;",
        )
        .unwrap();
    }
    let startup_path = home.join("startup.sock");
    let wait_path = home.join("read-wait.sock");
    let startup_listener = tokio::net::UnixListener::bind(&startup_path).unwrap();
    let wait_listener = tokio::net::UnixListener::bind(&wait_path).unwrap();
    let stdout_path = temp.path().join("references.stdout");
    let stderr_path = temp.path().join("references.stderr");
    let mut read = command(&root, &home, "references");
    read.env("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path)
        .env("TRELLIS_TEST_CLI_READ_WAIT_SOCKET", &wait_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            fs::File::create(&stdout_path).unwrap(),
        ))
        .stderr(std::process::Stdio::from(
            fs::File::create(&stderr_path).unwrap(),
        ));
    let mut child = tokio::process::Command::from(read)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut startup = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        let (mut startup, _) = startup_listener.accept().await.unwrap();
        startup.read_exact(&mut [0]).await.unwrap();
        let (mut waiting, _) = wait_listener.accept().await.unwrap();
        let mut arrived = [0];
        waiting.read_exact(&mut arrived).await.unwrap();
        assert_eq!(arrived, *b"W");
        startup
    })
    .await
    .expect("references did not reach the typed startup-read wait");
    // Both owner startup and the refused read have arrived before release.
    // Successful output now must come from the same CLI's state-change retry.
    assert!(child.try_wait().unwrap().is_none());
    assert!(fs::read(&stdout_path).unwrap().is_empty());
    startup.write_all(&[1]).await.unwrap();
    let status = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, child.wait())
        .await
        .expect("references did not complete after startup release")
        .unwrap();
    assert!(
        status.success(),
        "{}",
        fs::read_to_string(&stderr_path).unwrap()
    );
    let references: Value = serde_json::from_slice(&fs::read(&stdout_path).unwrap()).unwrap();
    assert_ne!(
        references["payload"]["indexGeneration"], indexed["publishedRevision"]["indexGeneration"],
        "old derived schema must be recreated, not read through"
    );
    assert_eq!(references["payload"]["indexRevision"], 1);
    assert_eq!(references["payload"]["counts"]["mappedSites"], 1);
    assert_eq!(
        references["payload"]["production"][0]["source"]["path"],
        "src/low.rs"
    );
    assert_eq!(
        references["payload"]["production"][0]["target"]["path"],
        "src/high.rs"
    );
}
