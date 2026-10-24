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
    assert!(report.status.success());
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["derived"][0]["status"], "busy");
}

fn write_optional_presentation(dir: &std::path::Path, label: &str, source_hash: &str) {
    use protobuf::Message;
    let mut index = scip::types::Index::new();
    let mut document = scip::types::Document::new();
    document.relative_path = "main.js".into();
    document.position_encoding =
        scip::types::PositionEncoding::UTF16CodeUnitOffsetFromLineStart.into();
    let mut occurrence = scip::types::Occurrence::new();
    occurrence.range = vec![0, 9, 10];
    occurrence.symbol_roles = 1;
    occurrence.symbol = label.into();
    document.occurrences.push(occurrence);
    index.documents.push(document);
    fs::write(dir.join("index.scip"), index.write_to_bytes().unwrap()).unwrap();
    fs::write(
        dir.join("manifest.json"),
        serde_json::to_vec(&serde_json::json!({"main.js":source_hash})).unwrap(),
    )
    .unwrap();
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
fn standalone_takeover_replays_original_relative_presentation_from_another_cwd() {
    use sha2::{Digest, Sha256};
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let first_cwd = temp.path().join("ingress");
    let second_cwd = temp.path().join("takeover");
    for dir in [&root, &first_cwd, &second_cwd] {
        fs::create_dir(dir).unwrap();
    }
    let ingress_cwd = first_cwd.canonicalize().unwrap();
    let source = "function f() {}\nf();\n";
    fs::write(root.join("main.js"), source).unwrap();
    let source_hash = hex::encode(Sha256::digest(source.as_bytes()));
    let original_label = "scip npm display 1 main.js/f().";
    write_optional_presentation(&first_cwd, original_label, &source_hash);
    // A cwd-rebound replay would use this same-name but wrong presentation.
    write_optional_presentation(&second_cwd, "scip npm display 1 other().", "stale");
    let indexed = command(&root, &home, "index")
        .current_dir(&first_cwd)
        .arg("--scip")
        .arg("index.scip")
        .arg("--manifest")
        .arg("manifest.json")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let initial: Value = serde_json::from_slice(&indexed.stdout).unwrap();
    // Startup reconciliation publishes r1. If its capture already used the
    // accepted request's SCIP options, the request is unchanged and completes
    // at r1 (Decision 0006 §3); otherwise it publishes r2 for the new inputs.
    assert!(
        (1..=2).contains(
            &initial["publishedRevision"]["indexRevision"]
                .as_u64()
                .unwrap()
        ),
        "{initial}"
    );
    let indexes = if cfg!(target_os = "macos") {
        home.join("Library/Caches/dev.squashmerge.trellis/indexes")
    } else {
        home.join(".cache/trellis/indexes")
    };
    let index_db = fs::read_dir(indexes)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let recorded_options = || -> Value {
        let db = rusqlite::Connection::open(&index_db).unwrap();
        let text: String = db
            .query_row(
                "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        serde_json::from_str(&text).unwrap()
    };
    let recorded = recorded_options();
    assert_eq!(
        recorded["scipPath"],
        ingress_cwd.join("index.scip").to_str().unwrap()
    );
    assert_eq!(
        recorded["manifestPath"],
        ingress_cwd.join("manifest.json").to_str().unwrap()
    );
    let status = command(&root, &home, "status")
        .current_dir(&second_cwd)
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "{}",
        String::from_utf8_lossy(&status.stderr)
    );
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["revision"], initial["publishedRevision"]);
    let graph = stopped_graph(&root, &home);
    let f = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "f")
        .unwrap();
    assert_eq!(f["displayLabel"], original_label);
    assert_eq!(
        recorded_options(),
        recorded,
        "takeovers must retain ingress identities"
    );
}

#[test]
fn standalone_takeover_keeps_configured_missing_presentation_absent() {
    use sha2::{Digest, Sha256};
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let first_cwd = temp.path().join("ingress");
    let second_cwd = temp.path().join("takeover");
    for dir in [&root, &first_cwd, &second_cwd] {
        fs::create_dir(dir).unwrap();
    }
    let source = "function f() {}\nf();\n";
    fs::write(root.join("main.js"), source).unwrap();
    let source_hash = hex::encode(Sha256::digest(source.as_bytes()));
    // Inputs with these names exist only in B, never in the configured A.
    write_optional_presentation(&second_cwd, "scip npm display 1 main.js/f().", &source_hash);
    let indexed = command(&root, &home, "index")
        .current_dir(&first_cwd)
        .arg("--scip")
        .arg("index.scip")
        .arg("--manifest")
        .arg("manifest.json")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert!(!first_cwd.join("index.scip").exists());
    assert!(!first_cwd.join("manifest.json").exists());
    let takeover = command(&root, &home, "status")
        .current_dir(&second_cwd)
        .output()
        .unwrap();
    assert!(
        takeover.status.success(),
        "{}",
        String::from_utf8_lossy(&takeover.stderr)
    );
    let graph = stopped_graph(&root, &home);
    let f = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "f")
        .unwrap();
    assert!(
        f["displayLabel"].is_null(),
        "absent A inputs must not become B labels"
    );
}

#[tokio::test]
async fn serve_ingress_persists_relative_presentation_paths_before_cross_cwd_takeover() {
    use sha2::{Digest, Sha256};
    use std::{os::unix::fs::PermissionsExt, process::Stdio};
    struct Server(Option<std::process::Child>);
    impl Drop for Server {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let _daemon = FixtureDaemon::new(&home);
    let first_cwd = temp.path().join("ingress");
    let second_cwd = temp.path().join("takeover");
    for dir in [&root, &first_cwd, &second_cwd] {
        fs::create_dir(dir).unwrap();
    }
    let ingress_cwd = first_cwd.canonicalize().unwrap();
    let source = "function f() {}\nf();\n";
    fs::write(root.join("main.js"), source).unwrap();
    let source_hash = hex::encode(Sha256::digest(source.as_bytes()));
    let original_label = "scip npm display 1 main.js/f().";
    write_optional_presentation(&first_cwd, original_label, &source_hash);
    write_optional_presentation(&second_cwd, "scip npm display 1 other().", "stale");
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let token = temp.path().join("token");
    fs::write(&token, TOKEN).unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    // Keep the daemon's stderr open through readiness and shutdown. Reading
    // only the first banner line from a pipe closed its reader while later
    // workers still wrote diagnostics on Linux CI.
    let stderr_path = temp.path().join("serve-ingress-stderr.log");
    let stderr_file = fs::File::create(&stderr_path).unwrap();
    let child = isolated_command(&home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .arg("serve")
        .arg("--workspace")
        .arg(&root)
        .arg("--bind")
        .arg(format!("127.0.0.1:{port}"))
        .arg("--token-file")
        .arg(&token)
        .arg("--scip")
        .arg("index.scip")
        .arg("--manifest")
        .arg("manifest.json")
        .current_dir(&first_cwd)
        .stdout(Stdio::null())
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .unwrap();
    let mut server = Server(Some(child));
    let startup = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let log = fs::read(&stderr_path).unwrap();
            if log
                .windows(b"Trellis:".len())
                .any(|window| window == b"Trellis:")
            {
                break log;
            }
            if let Some(exit) = server.0.as_mut().unwrap().try_wait().unwrap() {
                panic!(
                    "daemon exited before banner: {exit}; category={}",
                    readiness_stderr_category(&log)
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("daemon startup timed out");
    assert!(home.join("auto-daemon.pid").exists());
    assert!(
        !String::from_utf8_lossy(&startup).contains("Evidence unavailable at startup"),
        "unexpected startup evidence: {}",
        readiness_stderr_category(&startup)
    );
    // The banner precedes axum polling its SIGTERM handler; a successful
    // response, not a logged banner, proves the listener is actually ready.
    let mut last_health = String::from("no_response");
    let mut ready = false;
    let health = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            match reqwest::get(format!("http://127.0.0.1:{port}/healthz")).await {
                Ok(response) if response.status().is_success() => {
                    ready = true;
                    break;
                }
                Ok(response) => last_health = format!("http_status_{}", response.status().as_u16()),
                Err(error) => {
                    last_health = if error.is_connect() {
                        "connection_error".into()
                    } else if error.is_timeout() {
                        "request_timeout".into()
                    } else {
                        "other_request_error".into()
                    }
                }
            }
            if server.0.as_mut().unwrap().try_wait().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        health.is_ok() && ready,
        "daemon health endpoint did not become ready: child={:?}, last_health={}, stderr_category={}",
        server.0.as_mut().unwrap().try_wait().unwrap(),
        last_health,
        readiness_stderr_category(&fs::read(&stderr_path).unwrap())
    );
    // The banner precedes an asynchronous queue tick. SIGKILL can interrupt
    // requests.db creation before its schema transaction commits; that partial
    // queue must remain incompatible. Model a clean cross-CWD takeover instead:
    // SIGTERM lets the daemon finish in-flight blocking work before it exits.
    let pid = server.0.as_ref().unwrap().id();
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGTERM) }, 0);
    let deadline = tokio::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if let Some(status) = server.0.as_mut().unwrap().try_wait().unwrap() {
            // Reaped children must not be killed again by the cleanup guard.
            let _ = server.0.take();
            assert!(
                status.success(),
                "daemon failed graceful shutdown: {status}"
            );
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "daemon did not stop gracefully before cross-CWD takeover"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    drop(server);
    let takeover = command(&root, &home, "status")
        .current_dir(&second_cwd)
        .output()
        .unwrap();
    assert!(
        takeover.status.success(),
        "{}",
        String::from_utf8_lossy(&takeover.stderr)
    );
    let graph = stopped_graph(&root, &home);
    let f = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["name"] == "f")
        .unwrap();
    assert_eq!(f["displayLabel"], original_label);
    let indexes = if cfg!(target_os = "macos") {
        home.join("Library/Caches/dev.squashmerge.trellis/indexes")
    } else {
        home.join(".cache/trellis/indexes")
    };
    let index_db = fs::read_dir(indexes)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db");
    let db = rusqlite::Connection::open(&index_db).unwrap();
    let recorded: String = db
        .query_row(
            "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let recorded: Value = serde_json::from_str(&recorded).unwrap();
    assert_eq!(
        recorded["scipPath"],
        ingress_cwd.join("index.scip").to_str().unwrap()
    );
    assert_eq!(
        recorded["manifestPath"],
        ingress_cwd.join("manifest.json").to_str().unwrap()
    );
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
fn live_jev_requires_paired_explicit_budget_flags() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let state = temp.path().join("state");
    for flags in [
        vec!["--jev-budget-dir", "unused"],
        vec!["--jev-budget-cents", "500"],
        vec!["--jev-budget-dir", "unused", "--jev-budget-cents", "501"],
        vec!["--jev-budget-dir", "unused", "--jev-budget-cents", "0"],
    ] {
        let output = command(&source, &state, "serve")
            .args(flags)
            .env_remove("JEV_KEY")
            .output()
            .unwrap();
        assert!(!output.status.success());
    }
    assert!(
        !state.exists(),
        "invalid opt-in must fail before state creation"
    );
}

#[test]
fn live_jev_missing_key_fails_without_creating_budget() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let state = temp.path().join("state");
    let budget = temp.path().join("budget");
    let output = command(&source, &state, "serve")
        .args([
            "--bind",
            "127.0.0.1:0",
            "--jev-budget-cents",
            "500",
            "--jev-budget-dir",
        ])
        .arg(&budget)
        .env_remove("JEV_KEY")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("JEV_KEY must be configured"));
    assert!(!budget.exists());
}

#[cfg(unix)]
#[test]
fn malformed_jev_environment_never_echoes_credential_bytes() {
    use std::os::unix::ffi::OsStringExt;
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let budget = temp.path().join("budget");
    let output = command(&source, &temp.path().join("state"), "serve")
        .args([
            "--bind",
            "127.0.0.1:0",
            "--jev-budget-cents",
            "500",
            "--jev-budget-dir",
        ])
        .arg(&budget)
        .env(
            "JEV_KEY",
            std::ffi::OsString::from_vec(b"SYNTHETIC_INVALID_CREDENTIAL_\xff".to_vec()),
        )
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("JEV_KEY must be configured"));
    assert!(!stderr.contains("SYNTHETIC_INVALID_CREDENTIAL"));
    assert!(!budget.exists());
}

#[test]
fn acp_requires_complete_explicit_allowance_flags() {
    let temp = TempDir::new().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let state = temp.path().join("state");
    for (case, flags) in [
        vec!["--acp-runner", "unused"],
        vec!["--acp-state-dir", "unused"],
        vec!["--acp-max-attempts", "1"],
        vec![
            "--acp-runner",
            "unused",
            "--acp-state-dir",
            "unused",
            "--acp-max-attempts",
            "0",
        ],
        vec![
            "--acp-runner",
            "unused",
            "--acp-state-dir",
            "unused",
            "--acp-max-attempts",
            "21",
        ],
    ]
    .into_iter()
    .enumerate()
    {
        let output = bounded_invalid_cli(
            temp.path(),
            &format!("acp-{case}"),
            command(&source, &state, "serve").args(flags),
        );
        assert!(!output.status.success());
    }
    assert!(
        !state.exists(),
        "invalid opt-in must fail before state creation"
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
    assert_eq!(inventory["records"], serde_json::json!([]));
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
fn forget_cli_requires_confirmation_and_preserves_unrelated_state() {
    use trellis::{
        model::SavedView,
        store::topology::{DurableRecords, TopologyRoots, UseGuard, WorkspaceIdentity},
    };
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let root = temp.path().join("work");
    fs::create_dir(&root).unwrap();
    let data = home.join(if cfg!(target_os = "macos") {
        "Library/Application Support/dev.squashmerge.trellis"
    } else {
        ".local/share/trellis"
    });
    let roots = TopologyRoots::isolated_for_tests(home.join("cache"), data);
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let view: SavedView = serde_json::from_str(
        r#"{"id":"view1","title":"A view","query":{"seed":"symbol"},"pins":{}}"#,
    )
    .unwrap();
    let records = DurableRecords::new(&roots, &identity);
    records.put_view(&view).unwrap();
    let id = identity.record_id.as_str();
    let run = |which: &str, yes: bool| {
        let mut cmd = isolated_command(&home);
        cmd.arg("forget").arg(which);
        if yes {
            cmd.arg("--yes");
        }
        cmd.output().unwrap()
    };
    assert!(!run("../work", true).status.success());
    let denied = run(id, false);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("terminal or --yes"));
    assert!(roots.record_db(&identity).exists());
    let shared =
        UseGuard::acquire_existing(&roots.record_use_lock(&identity), false, true).unwrap();
    assert!(!run(id, true).status.success());
    drop(shared);
    let other = home.join("untouched");
    fs::write(&other, "still here").unwrap();
    let result = run(id, true);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("every checkout sharing this UUID"));
    assert!(stderr.contains("Saved views: 1; annotations: 0"));
    assert!(stderr.contains(identity.root.to_str().unwrap()));
    assert!(!roots.record_dir(&identity).exists());
    assert!(!roots.record_use_lock(&identity).exists());
    assert_eq!(fs::read_to_string(other).unwrap(), "still here");
    assert!(!run(id, true).status.success());
}

#[test]
fn forget_yes_refuses_unknown_sqlite_schema_without_removing_state() {
    use trellis::{
        model::SavedView,
        store::topology::{DurableRecords, TopologyRoots, WorkspaceIdentity},
    };
    let temp = TempDir::new().unwrap();
    let home = temp.path().join("home");
    let work = temp.path().join("work");
    fs::create_dir(&work).unwrap();
    let data = home.join(if cfg!(target_os = "macos") {
        "Library/Application Support/dev.squashmerge.trellis"
    } else {
        ".local/share/trellis"
    });
    let roots = TopologyRoots::isolated_for_tests(home.join("cache"), data);
    let identity = WorkspaceIdentity::discover(Some(&work), &work).unwrap();
    let view: SavedView = serde_json::from_str(
        r#"{"id":"view1","title":"A view","query":{"seed":"symbol"},"pins":{}}"#,
    )
    .unwrap();
    DurableRecords::new(&roots, &identity)
        .put_view(&view)
        .unwrap();
    let path = roots.record_db(&identity);
    let lock = roots.record_use_lock(&identity);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE unknown(id INTEGER)")
        .unwrap();
    drop(db);
    let before = fs::read(&path).unwrap();
    let lock_before = fs::read(&lock).unwrap();
    let unrelated = home.join("unrelated");
    fs::write(&unrelated, "keep").unwrap();
    let result = isolated_command(&home)
        .args(["forget", &identity.record_id, "--yes"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("unexpected SQLite schema"));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&lock).unwrap(), lock_before);
    assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
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
    let mut names = db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE '%trigrams%' AND name!='native_binding_epoch' AND name!='native_revision_supersessions' AND (name LIKE 'native_%' OR name IN ('document_versions','revision_documents','revision_capture_inputs','graph_projections','graph_nodes','graph_display_labels','class_projections','classes','class_relations')) ORDER BY name").unwrap();
    for name in names.query_map([], |r| r.get::<_, String>(0)).unwrap() {
        let name = name.unwrap();
        let (predicate, alias) = match name.as_str() {
            "native_revisions" => ("id=?1", ""),
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
            _ => panic!("unexpected v9 evidence table: {name}"),
        };
        let mut table = db
            .prepare(&format!(
                "SELECT * FROM \"{name}\"{alias} WHERE {predicate} ORDER BY rowid"
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
    assert_eq!(
        all_rows.len(),
        34,
        "retain every native evidence table in schema-17 raw snapshot"
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
    assert!(header[13].as_i64().is_some_and(|revision| revision > 0));
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
    header[13] = serde_json::json!("<current-revision>");
    for table in ["revision_capture_inputs", "revision_documents"] {
        for row in evidence["allRows"][table].as_array_mut().unwrap() {
            row[0] = serde_json::json!("<current-pin>");
        }
    }
    evidence
}

// The provisioned browser serves global control-plane routes only. Keep the
// selected-workspace API contract on the legacy router, backed by the very
// same HOME topology as the executable fixtures above.
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
    fn same_home(
        root: &std::path::Path,
        home: &std::path::Path,
        token: &str,
        url: &str,
        max_file_bytes: u64,
    ) -> Self {
        Self::same_home_with_session(root, home, token, url, max_file_bytes, true)
    }
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
            origin: None,
            body: None,
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
    origin: Option<String>,
    body: Option<Value>,
}
impl LegacyRequest {
    fn header(mut self, name: &str, value: &str) -> Self {
        assert!(name.eq_ignore_ascii_case("origin"));
        self.origin = Some(value.to_owned());
        self
    }
    fn bearer_auth(mut self, token: &str) -> Self {
        self.token = Some(token.to_owned());
        self
    }
    fn json(mut self, body: &Value) -> Self {
        self.body = Some(body.clone());
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
        if let Some(origin) = self.origin {
            request = request.header("origin", origin);
        }
        let body = if let Some(value) = self.body {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        } else {
            Body::empty()
        };
        let response = self.app.oneshot(request.body(body)?).await?;
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

async fn real_api(
    client: &LegacyClient,
    url: &str,
    token: &str,
    method: reqwest::Method,
    route: &str,
    body: Option<Value>,
) -> (reqwest::StatusCode, Value) {
    let mut request = client
        .request(method, format!("{url}{route}"))
        .header("Origin", url)
        .bearer_auth(token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.unwrap();
    let status = response.status();
    (status, response.json().await.unwrap())
}

async fn saved_item_status(request: LegacyRequest, context: &str) -> anyhow::Result<Value> {
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

fn saved_item_status_fixture(
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
            origin: None,
            body: None,
        },
        attempts,
    )
}

#[tokio::test]
async fn saved_item_status_retries_only_typed_startup_refusals() {
    use reqwest::StatusCode;
    let (request, attempts) = saved_item_status_fixture(vec![
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
    let body = saved_item_status(request, "initial").await.unwrap();
    assert_eq!(body["catchingUp"], true);
    assert_eq!(body["revision"]["indexRevision"], 1);
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn saved_item_status_rejects_other_responses_with_the_body() {
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
        let (request, attempts) = saved_item_status_fixture(vec![(status, body)]);
        let error = saved_item_status(request, "reopened")
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

struct SavedItemServer;

async fn start_saved_item_server(
    _temp: &std::path::Path,
    root: &std::path::Path,
    home: &std::path::Path,
    token: &str,
) -> (SavedItemServer, LegacyClient, String) {
    // The preceding CLI index elected a real daemon. End this fixture's
    // writer before the direct router takes over the same HOME-backed Store.
    let marker = home.join("auto-daemon.pid");
    if marker.exists() {
        // FixtureDaemon checks the live socket peer and exact executable
        // before TERM; a numeric marker alone is not process ownership.
        drop(FixtureDaemon::new(home));
        let data = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/dev.squashmerge.trellis")
        } else {
            home.join(".local/share/trellis")
        };
        let socket = trellis::daemon::SocketPaths::new(&data).socket;
        let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
        while std::os::unix::net::UnixStream::connect(&socket).is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "prior saved-item fixture daemon did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!marker.exists(), "fixture daemon was not safely stopped");
    }
    let url = "http://127.0.0.1:7331".to_owned();
    (
        SavedItemServer,
        LegacyClient::same_home(root, home, token, &url, 2_097_152),
        url,
    )
}

async fn real_index_job(
    client: &LegacyClient,
    url: &str,
    token: &str,
    expected: &Value,
    changed_source: &str,
) -> Value {
    use std::time::Duration;
    let (status, accepted) = real_api(
        client,
        url,
        token,
        reqwest::Method::POST,
        "/api/index",
        Some(serde_json::json!({"expectedRevision":expected})),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::ACCEPTED, "{accepted}");
    let id = accepted["id"].as_str().unwrap();
    let completed = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (status, job) = real_api(
                client,
                url,
                token,
                reqwest::Method::GET,
                &format!("/api/jobs/{id}"),
                None,
            )
            .await;
            assert_eq!(status, reqwest::StatusCode::OK, "{job}");
            if !job["finishedAt"].is_null() {
                break job;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("saved-item index job");
    if completed["state"] == "done" {
        let pin = completed["revision"].clone();
        assert_eq!(pin["indexGeneration"], expected["indexGeneration"]);
        assert!(
            pin["indexRevision"].as_u64().unwrap() > expected["indexRevision"].as_u64().unwrap()
        );
        let (status, current) = real_api(
            client,
            url,
            token,
            reqwest::Method::GET,
            "/api/status",
            None,
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{current}");
        assert_eq!(
            current["revision"], pin,
            "completed job must select its returned pin"
        );
        return pin;
    }
    // The direct router's watcher can publish the edited source first. A stale
    // explicit job must remain failed; prove that its replacement selected pin
    // contains attested changed source bytes before using it below.
    assert_eq!(
        completed["error"]["code"], "revision_conflict",
        "{completed}"
    );
    let (status, current) = real_api(
        client,
        url,
        token,
        reqwest::Method::GET,
        "/api/status",
        None,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{current}");
    let revision = &current["revision"];
    assert_eq!(revision["indexGeneration"], expected["indexGeneration"]);
    assert!(
        revision["indexRevision"].as_u64().unwrap() > expected["indexRevision"].as_u64().unwrap(),
        "failed job without a newer selected head: {completed}; current={current}"
    );
    let route = saved_pin_route("/api/source?path=flow.rs", revision);
    let (status, source) = real_api(client, url, token, reqwest::Method::GET, &route, None).await;
    assert_eq!(status, reqwest::StatusCode::OK, "{source}");
    assert_eq!(source["revision"], *revision);
    assert_eq!(
        source["file"]["text"], changed_source,
        "attested changed source at selected pin"
    );
    revision.clone()
}

fn saved_pin_route(route: &str, pin: &Value) -> String {
    let separator = if route.contains('?') { '&' } else { '?' };
    format!(
        "{route}{separator}indexGeneration={}&indexRevision={}",
        pin["indexGeneration"].as_str().unwrap(),
        pin["indexRevision"].as_u64().unwrap()
    )
}

fn find_workspace_db(home: &std::path::Path) -> Option<std::path::PathBuf> {
    fn find(dir: &std::path::Path) -> Option<std::path::PathBuf> {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "workspace.db") {
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
    find(home)
}

fn stored_payload(db_path: &std::path::Path, table: &str, id: &str) -> String {
    assert!(matches!(table, "views" | "annotations"));
    let db = rusqlite::Connection::open(db_path).unwrap();
    db.query_row(
        &format!("SELECT payload FROM {table} WHERE id=?1"),
        [id],
        |row| row.get(0),
    )
    .unwrap()
}

fn raw_anchor(payload: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Envelope {
        #[serde(default)]
        anchor: Option<Box<serde_json::value::RawValue>>,
    }
    serde_json::from_str::<Envelope>(payload)
        .unwrap()
        .anchor
        .map(|raw| raw.get().to_owned())
}

// Reverse the lossless Jev table encoding to check the real cached export against
// the separately read graph and preview, not only its HTTP status or packet ID.
fn decoded_jev_rows(export: &Value, table: &str) -> Vec<Value> {
    fn identity(cell: &Value, identities: &[Value]) -> Value {
        match cell {
            Value::Null => Value::Null,
            Value::Number(n) => identities[n.as_u64().unwrap() as usize].clone(),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| identity(v, identities)).collect())
            }
            _ => panic!("unexpected Jev identity cell: {cell}"),
        }
    }
    let state = &export["state"];
    let encoded = &state["packet"]["context"][table];
    let identities = state["identities"].as_array().unwrap();
    let columns = encoded["columns"].as_array().unwrap();
    let identity_columns = encoded["identityColumns"].as_array().unwrap();
    let range_columns = state["rangeColumns"].as_array().unwrap();
    encoded["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            let cells = row.as_array().unwrap();
            assert_eq!(cells.len(), columns.len());
            let mut result = serde_json::Map::new();
            for (column, cell) in columns.iter().zip(cells) {
                let name = column.as_str().unwrap();
                let decoded = if identity_columns.iter().any(|id| id.as_str() == Some(name)) {
                    identity(cell, identities)
                } else if name == "range" {
                    let values = cell.as_array().unwrap();
                    assert_eq!(values.len(), range_columns.len());
                    Value::Object(
                        range_columns
                            .iter()
                            .zip(values)
                            .map(|(key, value)| (key.as_str().unwrap().to_owned(), value.clone()))
                            .collect(),
                    )
                } else {
                    cell.clone()
                };
                result.insert(name.to_owned(), decoded);
            }
            Value::Object(result)
        })
        .collect()
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
        // Cache a selected question and source at the committed head. The
        // unchanged request publishes nothing, so the source stays readable.
        let prior_selection = if file.is_empty() {
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
            let (code, symbols) = real_selected_api(
                &client,
                &url,
                &origin,
                token,
                reqwest::Method::GET,
                "/symbols?q=seed",
                None,
            )
            .await;
            assert_eq!(code, 200, "{name}: prior selected symbols: {symbols}");
            assert_eq!(symbols["revision"], post_start_pin, "{name}");
            let seed = symbols["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "seed")
                .expect("prior selected seed")["id"]
                .as_str()
                .unwrap();
            let (code, preview) = real_selected_api(
                &client, &url, &origin, token, reqwest::Method::POST, "/questions/preview",
                Some(serde_json::json!({"seed":seed,"question":"what happens?","expectedRevision":post_start_pin}))
            ).await;
            assert_eq!(code, 200, "{name}: prior selected question: {preview}");
            assert_eq!(preview["packet"]["revision"], post_start_pin, "{name}");
            let packet = preview["packet"]["packetId"].as_str().unwrap();
            let packet_route = format!("/questions/{packet}/jev-request");
            let (code, packet_request) = real_selected_api(
                &client,
                &url,
                &origin,
                token,
                reqwest::Method::GET,
                &packet_route,
                None,
            )
            .await;
            assert_eq!(code, 200, "{name}: prior packet request: {packet_request}");
            assert_eq!(
                packet_request["state"]["packet"]["revision"], post_start_pin,
                "{name}"
            );
            Some((source_route, packet_route))
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
        if let Some((source_route, packet_route)) = &prior_selection {
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
            // Every drained request resets the packet cache, even one that
            // keeps the head, so the packet is evicted rather than stale.
            let (code, evicted_packet) = real_selected_api(
                &client,
                &url,
                &origin,
                token,
                reqwest::Method::GET,
                packet_route,
                None,
            )
            .await;
            assert!(
                code == reqwest::StatusCode::NOT_FOUND
                    && evicted_packet["error"]["code"] == "not_found",
                "{name}: evicted packet must be safely refused: {code} {evicted_packet}"
            );
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
            let symbols = "/symbols?q=seed".to_owned();
            let response = fetch(reqwest::Method::GET, symbols.clone(), None).await;
            let seed = response.1["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "seed")
                .expect("measured selected seed")["id"]
                .as_str()
                .unwrap()
                .to_owned();
            captured.insert(symbols, response);
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
            let preview = fetch(reqwest::Method::POST, "/questions/preview".into(),
                Some(serde_json::json!({"seed":seed,"question":"what happens?","expectedRevision":pin}))).await;
            let packet = preview.1["packet"]["packetId"]
                .as_str()
                .expect("selected question packet");
            let export = format!("/questions/{packet}/jev-request");
            captured.insert(
                export.clone(),
                fetch(reqwest::Method::GET, export, None).await,
            );
            captured.insert("/questions/preview".into(), preview);
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
        assert_eq!(after_header[13], pin["indexRevision"], "{name}");
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
            "{name}: all 34 native schema-17 evidence tables and documents must match after only publication identity normalization"
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

            let (code, symbols) = captured
                .get("/symbols?q=seed")
                .expect("selected HTTP response")
                .clone();
            assert_eq!(code, 200, "{name}: {symbols}");
            assert_eq!(symbols["revision"], pin, "{name}");
            let seed = symbols["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["name"] == "seed")
                .expect("measured seed declaration")["id"]
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
            let expected_sequence = trellis::behavior::build_sequence(
                expected_pin,
                &expected_seed,
                &expected_file,
                &expected_calls,
                false,
            )
            .unwrap();
            assert_eq!(
                sequence,
                serde_json::to_value(&expected_sequence).unwrap(),
                "{name}: complete sequence wire matches the captured source projection"
            );

            let (code, preview) = captured
                .get("/questions/preview")
                .expect("selected HTTP response")
                .clone();
            assert_eq!(code, 200, "{name}: {preview}");
            let preview_packet = &preview["packet"];
            assert_eq!(preview_packet["revision"], pin, "{name}");
            assert_eq!(preview_packet["request"]["expectedRevision"], pin, "{name}");
            assert_eq!(preview_packet["request"]["seed"], seed, "{name}");
            assert_eq!(preview_packet["context"]["revision"], pin, "{name}");
            assert_eq!(preview_packet["context"]["query"]["seed"], seed, "{name}");
            assert_eq!(
                preview_packet["context"]["nodes"],
                serde_json::json!([measured_seed]),
                "{name}: packet must contain the exact selected graph/native seed"
            );
            assert_eq!(
                preview_packet["sourceFiles"],
                serde_json::json!([graph_files[0]]),
                "{name}: packet must cite exact selected captured source"
            );
            let selected_calls = preview_packet["context"]["calls"].as_array().unwrap();
            assert_eq!(
                selected_calls.len(),
                1,
                "{name}: exactly one preview sink call"
            );
            assert_eq!(
                selected_calls[0], *call,
                "{name}: preview must cite the full measured native/graph call"
            );
            assert!(
                graph_after["regions"].as_array().unwrap().is_empty(),
                "{name}: simple seed-to-sink fixture has no control region"
            );
            assert_eq!(
                preview_packet["context"]["regions"], graph_after["regions"],
                "{name}: no fabricated preview control regions"
            );
            assert!(!preview.to_string().contains("lexical-guess"), "{name}");
            let packet = preview_packet["packetId"].as_str().unwrap();

            let (code, export) = captured
                .get(&format!("/questions/{packet}/jev-request"))
                .expect("selected HTTP response")
                .clone();
            assert_eq!(code, 200, "{name}: {export}");
            assert_eq!(
                export["state"]["identityPaths"],
                serde_json::json!(["/request/seed", "/context/query/seed"]),
                "{name}: canonical identity paths"
            );
            assert_eq!(
                export["state"]["rangeColumns"],
                serde_json::json!([
                    "startByte",
                    "endByte",
                    "startLine",
                    "startColumn",
                    "endLine",
                    "endColumn"
                ]),
                "{name}: canonical source range columns"
            );
            for (table, columns) in [
                ("nodes", serde_json::json!(["id", "parent"])),
                ("calls", serde_json::json!(["id", "caller", "regions"])),
                ("regions", serde_json::json!(["id", "parent", "owner"])),
            ] {
                assert_eq!(
                    export["state"]["packet"]["context"][table]["identityColumns"], columns,
                    "{name}: canonical {table} identity columns"
                );
            }
            let wire = &export["state"]["packet"];
            assert_eq!(
                wire["packetId"], packet,
                "{name}: cached export packet identity"
            );
            assert_eq!(wire["revision"], pin, "{name}");
            assert_eq!(wire["request"]["expectedRevision"], pin, "{name}");
            assert_eq!(wire["context"]["revision"], pin, "{name}");
            assert_eq!(
                wire["sourceFiles"], preview_packet["sourceFiles"],
                "{name}: cached source witness"
            );
            let identities = export["state"]["identities"].as_array().unwrap();
            for index in [
                wire["request"]["seed"].as_u64().unwrap(),
                wire["context"]["query"]["seed"].as_u64().unwrap(),
            ] {
                assert_eq!(
                    identities[index as usize], seed,
                    "{name}: encoded seed identity"
                );
            }
            let mut decoded = wire.clone();
            decoded["request"]["seed"] = serde_json::json!(seed);
            decoded["context"]["query"]["seed"] = serde_json::json!(seed);
            for table in ["nodes", "calls", "regions"] {
                let rows = decoded_jev_rows(&export, table);
                if table == "nodes" || table == "calls" {
                    assert!(
                        !rows.is_empty(),
                        "{name}: cached {table} witness cannot be empty"
                    );
                }
                decoded["context"][table] = serde_json::json!(rows);
            }
            assert_eq!(
                decoded, *preview_packet,
                "{name}: lossless cached export must equal the entire selected measured packet"
            );
            let question_keys: Vec<_> = export["questions"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            let mut expected_keys: Vec<_> = (0..selected_calls.len())
                .map(|i| format!("c{i}_{packet}"))
                .collect();
            expected_keys.sort();
            assert!(
                !expected_keys.is_empty(),
                "{name}: expected measured seed call questions"
            );
            assert_eq!(
                question_keys, expected_keys,
                "{name}: cached questions bind selected packet/calls"
            );
            assert!(!export.to_string().contains("lexical-guess"), "{name}");
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

#[tokio::test]
async fn saved_items_real_index_matrix() {
    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("workspace");
    let home = temp.path().join("home");
    fs::create_dir(&root).unwrap();
    let source_path = root.join("flow.rs");
    fs::write(
        &source_path,
        "fn seed(value: i32) { sink(); }\nfn sink() {}\nfn other() {}\n",
    )
    .unwrap();
    let indexed = command(&root, &home, "index")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let indexed: Value = serde_json::from_slice(&indexed.stdout).unwrap();
    let initial_pin = indexed["status"]["revision"].clone();
    assert!(home.join("auto-daemon.pid").exists());
    let (_server, client, url) = start_saved_item_server(temp.path(), &root, &home, TOKEN).await;
    let serving_status = saved_item_status(
        client
            .get(format!("{url}/api/status"))
            .header("Origin", &url)
            .bearer_auth(TOKEN),
        "initial",
    )
    .await
    .unwrap();
    let pin = serving_status["revision"].clone();
    assert_eq!(pin["indexGeneration"], initial_pin["indexGeneration"]);
    // The direct router may commit mandatory startup reconciliation after the daemon exits.
    assert!(
        pin["indexRevision"].as_u64().unwrap() >= initial_pin["indexRevision"].as_u64().unwrap()
    );

    for route in ["/api/views", "/api/views/absent", "/api/annotations"] {
        let (status, response) =
            real_api(&client, &url, TOKEN, reqwest::Method::GET, route, None).await;
        if route.ends_with("absent") {
            assert_eq!(status, reqwest::StatusCode::NOT_FOUND, "{response}");
        } else {
            assert_eq!(status, reqwest::StatusCode::OK, "{response}");
            assert_eq!(response, serde_json::json!([]));
        }
    }
    assert!(
        find_workspace_db(&home).is_none(),
        "saved reads before first write must not create workspace.db"
    );

    let (status, symbols) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::GET,
        "/api/symbols?q=",
        None,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{symbols}");
    let seed = symbols["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "seed")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let other = symbols["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "other")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let view_body = serde_json::json!({
        "id":"real-view","title":"Original","query":{"seed":seed}
    });
    let (status, saved_view) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::PUT,
        &saved_pin_route("/api/views/real-view", &pin),
        Some(view_body.clone()),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{saved_view}");
    assert_eq!(saved_view["indexGeneration"], pin["indexGeneration"]);
    assert_eq!(saved_view["indexRevision"], pin["indexRevision"]);
    assert_eq!(saved_view["view"]["query"]["seed"], seed);
    assert_eq!(saved_view["view"]["pins"], serde_json::json!({}));
    assert_eq!(saved_view["view"]["hidden"], serde_json::json!([]));
    assert_eq!(saved_view["attachment"]["availability"], "ready");
    assert_eq!(saved_view["attachment"]["result"]["status"], "attached");
    assert_eq!(saved_view["attachment"]["result"]["targetId"], seed);

    let note_body = serde_json::json!({
        "id":"real-note","nodeId":seed,"body":"First body","title":"First title"
    });
    let (status, saved_note) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::PUT,
        &saved_pin_route("/api/annotations/real-note", &pin),
        Some(note_body.clone()),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{saved_note}");
    assert_eq!(saved_note["indexGeneration"], pin["indexGeneration"]);
    assert_eq!(saved_note["indexRevision"], pin["indexRevision"]);
    assert_eq!(saved_note["annotation"]["nodeId"], seed);
    assert_eq!(saved_note["annotation"]["title"], "First title");
    assert_eq!(saved_note["attachment"]["availability"], "ready");
    assert_eq!(saved_note["attachment"]["result"]["targetId"], seed);

    let record_db = find_workspace_db(&home).expect("schema-1 saved record database");
    let record = rusqlite::Connection::open(&record_db).unwrap();
    let metadata: (i64, i64) = record
        .query_row(
            "SELECT schema_version,initialized FROM record_metadata WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(metadata, (1, 1), "saved payload is schema 1");
    let record_version: i64 = record
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(record_version, 1);
    drop(record);
    let original_view_payload = stored_payload(&record_db, "views", "real-view");
    let original_note_payload = stored_payload(&record_db, "annotations", "real-note");
    let original_view_anchor = raw_anchor(&original_view_payload).unwrap();
    let original_note_anchor = raw_anchor(&original_note_payload).unwrap();
    for (payload, target) in [
        (&original_view_payload, &seed),
        (&original_note_payload, &seed),
    ] {
        let value: Value = serde_json::from_str(payload).unwrap();
        let fields = value["anchor"].as_object().unwrap();
        let mut keys = fields.keys().map(String::as_str).collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "capturedRevisionId",
                "document",
                "headerHash",
                "identicalHeaderCount",
                "siblingCount",
                "siblingGroupHash",
                "syntaxId",
            ]
        );
        assert_eq!(fields["syntaxId"], target.as_str());
        assert_eq!(fields["headerHash"].as_str().unwrap().len(), 64);
        assert_eq!(fields["siblingGroupHash"].as_str().unwrap().len(), 64);
    }
    let index_version: i64 = rusqlite::Connection::open(real_index_db(&home))
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(
        index_version, 17,
        "derived v17 index inspection is separate"
    );

    let edited_view_body = serde_json::json!({
        "id":"real-view","title":"Edited","query":{"seed":seed}
    });
    let (status, edited_view) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::PUT,
        &saved_pin_route("/api/views/real-view", &pin),
        Some(edited_view_body.clone()),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{edited_view}");
    assert_eq!(edited_view["view"]["anchor"], saved_view["view"]["anchor"]);
    let edited_note_body = serde_json::json!({
        "id":"real-note","nodeId":seed,"body":"Edited body"
    });
    let (status, edited_note) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::PUT,
        &saved_pin_route("/api/annotations/real-note", &pin),
        Some(edited_note_body.clone()),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{edited_note}");
    assert_eq!(edited_note["annotation"]["title"], "First title");
    assert_eq!(
        edited_note["annotation"]["anchor"],
        saved_note["annotation"]["anchor"]
    );
    assert_eq!(
        raw_anchor(&stored_payload(&record_db, "views", "real-view")).as_deref(),
        Some(original_view_anchor.as_str())
    );
    assert_eq!(
        raw_anchor(&stored_payload(&record_db, "annotations", "real-note")).as_deref(),
        Some(original_note_anchor.as_str())
    );

    for (route, body) in [
        (
            saved_pin_route("/api/views/real-view", &pin),
            serde_json::json!({
                "id":"real-view","title":"Injected","query":{"seed":seed},
                "anchor":saved_view["view"]["anchor"]
            }),
        ),
        (
            saved_pin_route("/api/annotations/real-note", &pin),
            serde_json::json!({
                "id":"real-note","nodeId":seed,"body":"Injected",
                "attachment":saved_note["attachment"]
            }),
        ),
        (
            saved_pin_route("/api/views/real-view", &pin),
            serde_json::json!({"id":"real-view","title":"Retarget","query":{"seed":other}}),
        ),
        (
            saved_pin_route("/api/annotations/real-note", &pin),
            serde_json::json!({"id":"real-note","nodeId":other,"body":"Retarget"}),
        ),
    ] {
        let (status, error) = real_api(
            &client,
            &url,
            TOKEN,
            reqwest::Method::PUT,
            &route,
            Some(body),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{error}");
    }
    assert_eq!(
        raw_anchor(&stored_payload(&record_db, "views", "real-view")).as_deref(),
        Some(original_view_anchor.as_str())
    );
    assert_eq!(
        raw_anchor(&stored_payload(&record_db, "annotations", "real-note")).as_deref(),
        Some(original_note_anchor.as_str())
    );

    let changed_source = "fn seed(value: i64) { sink(); }\nfn sink() {}\nfn other() {}\n";
    fs::write(&source_path, changed_source).unwrap();
    let next = real_index_job(&client, &url, TOKEN, &pin, changed_source).await;
    assert_eq!(next["indexGeneration"], pin["indexGeneration"]);
    assert!(next["indexRevision"].as_u64().unwrap() > pin["indexRevision"].as_u64().unwrap());

    let routes = [
        saved_pin_route("/api/views/real-view", &next),
        saved_pin_route("/api/views", &next),
        saved_pin_route("/api/annotations", &next),
    ];
    for route in routes {
        let (status, response) =
            real_api(&client, &url, TOKEN, reqwest::Method::GET, &route, None).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{route}: {response}");
        let state = if route.contains("/real-view") {
            &response
        } else if route.contains("annotations") {
            &response[0]
        } else {
            response
                .as_array()
                .unwrap()
                .iter()
                .find(|state| state["view"]["id"] == "real-view")
                .unwrap()
        };
        assert_eq!(state["indexGeneration"], next["indexGeneration"]);
        assert_eq!(state["indexRevision"], next["indexRevision"]);
        assert_eq!(state["attachment"]["availability"], "ready");
        assert_eq!(state["attachment"]["result"]["status"], "orphaned");
        assert_eq!(state["attachment"]["result"]["reason"], "headerMismatch");
        assert!(state["attachment"]["result"]["targetId"].is_null());
        if route.contains("annotations") {
            assert_eq!(state["annotation"]["nodeId"], seed);
            assert!(state["orphaned"].as_bool().unwrap());
        } else {
            assert_eq!(state["view"]["query"]["seed"], seed);
            assert!(
                state["orphanedIds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|id| id == &serde_json::json!(seed))
            );
        }
    }
    assert_eq!(
        raw_anchor(&stored_payload(&record_db, "views", "real-view")).as_deref(),
        Some(original_view_anchor.as_str())
    );
    assert_eq!(
        raw_anchor(&stored_payload(&record_db, "annotations", "real-note")).as_deref(),
        Some(original_note_anchor.as_str())
    );

    for route in [
        saved_pin_route("/api/views/real-view", &pin),
        saved_pin_route("/api/views", &pin),
        saved_pin_route("/api/annotations", &pin),
    ] {
        let (status, response) =
            real_api(&client, &url, TOKEN, reqwest::Method::GET, &route, None).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{route}: {response}");
        let selected = if route.contains("/real-view") {
            &response
        } else {
            &response[0]
        };
        assert_eq!(selected["indexGeneration"], pin["indexGeneration"]);
        assert_eq!(selected["indexRevision"], pin["indexRevision"]);
        assert_eq!(selected["attachment"]["result"]["status"], "attached");
        assert_eq!(selected["attachment"]["result"]["targetId"], seed);
    }
    for (route, body) in [
        (
            saved_pin_route("/api/views/real-view", &pin),
            edited_view_body,
        ),
        (
            saved_pin_route("/api/annotations/real-note", &pin),
            edited_note_body,
        ),
    ] {
        let (status, error) = real_api(
            &client,
            &url,
            TOKEN,
            reqwest::Method::PUT,
            &route,
            Some(body),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::CONFLICT, "{error}");
    }

    let query = serde_json::json!({"seed":seed});
    let (status, stale_query) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::POST,
        &saved_pin_route("/api/query", &pin),
        Some(query.clone()),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{stale_query}");
    assert_eq!(stale_query["revision"], pin, "retained query must select P");
    let (status, current_query) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::POST,
        &saved_pin_route("/api/query", &next),
        Some(query.clone()),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{current_query}");
    assert_eq!(current_query["revision"], next);
    assert_eq!(current_query["nodes"][0]["id"], seed);
    let (status, ordinary_query) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::POST,
        "/api/query",
        Some(query),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{ordinary_query}");
    assert_eq!(ordinary_query["revision"], next);

    let orphan_view_edit = serde_json::json!({
        "id":"real-view","title":"Edited while orphaned","query":{"seed":seed}
    });
    let (status, orphan_view) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::PUT,
        &saved_pin_route("/api/views/real-view", &next),
        Some(orphan_view_edit),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{orphan_view}");
    assert_eq!(
        orphan_view["attachment"]["result"]["reason"],
        "headerMismatch"
    );
    let orphan_note_edit = serde_json::json!({
        "id":"real-note","nodeId":seed,"body":"Edited while orphaned","title":"New title"
    });
    let (status, orphan_note) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::PUT,
        &saved_pin_route("/api/annotations/real-note", &next),
        Some(orphan_note_edit),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{orphan_note}");
    assert_eq!(
        orphan_note["attachment"]["result"]["reason"],
        "headerMismatch"
    );
    assert_eq!(
        raw_anchor(&stored_payload(&record_db, "views", "real-view")).as_deref(),
        Some(original_view_anchor.as_str())
    );
    assert_eq!(
        raw_anchor(&stored_payload(&record_db, "annotations", "real-note")).as_deref(),
        Some(original_note_anchor.as_str())
    );

    drop(client);
    let db = rusqlite::Connection::open(&record_db).unwrap();
    db.execute(
        "INSERT INTO views(id,payload) VALUES(?1,?2)",
        rusqlite::params![
            "legacy-view",
            serde_json::json!({
                "id":"legacy-view","title":"Legacy","query":{"seed":seed},
                "pins":{},"hidden":[]
            })
            .to_string()
        ],
    )
    .unwrap();
    db.execute(
        "INSERT INTO annotations(id,node_id,payload) VALUES(?1,?2,?3)",
        rusqlite::params![
            "legacy-note",
            seed,
            serde_json::json!({"id":"legacy-note","nodeId":seed,"body":"Legacy body"}).to_string()
        ],
    )
    .unwrap();
    drop(db);

    let (_server, client, url) = start_saved_item_server(temp.path(), &root, &home, TOKEN).await;
    let reopened_status = saved_item_status(
        client
            .get(format!("{url}/api/status"))
            .header("Origin", &url)
            .bearer_auth(TOKEN),
        "reopened",
    )
    .await
    .unwrap();
    let reopened_pin = reopened_status["revision"].clone();
    assert_eq!(reopened_pin["indexGeneration"], next["indexGeneration"]);
    // Reopening the direct router may publish mandatory startup reconciliation again.
    assert!(
        reopened_pin["indexRevision"].as_u64().unwrap() >= next["indexRevision"].as_u64().unwrap()
    );
    let (status, views) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::GET,
        &saved_pin_route("/api/views", &reopened_pin),
        None,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{views}");
    let reopened = views
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["view"]["id"] == "real-view")
        .unwrap();
    assert_eq!(reopened["view"]["title"], "Edited while orphaned");
    assert_eq!(reopened["attachment"]["result"]["reason"], "headerMismatch");
    let legacy_view = views
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["view"]["id"] == "legacy-view")
        .unwrap();
    assert!(legacy_view["view"].get("anchor").is_none());
    assert_eq!(legacy_view["attachment"]["availability"], "anchorless");
    let (status, notes) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::GET,
        &saved_pin_route("/api/annotations", &reopened_pin),
        None,
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{notes}");
    let reopened_note = notes
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["annotation"]["id"] == "real-note")
        .unwrap();
    assert_eq!(reopened_note["annotation"]["title"], "New title");
    assert_eq!(reopened_note["annotation"]["body"], "Edited while orphaned");
    let legacy_note = notes
        .as_array()
        .unwrap()
        .iter()
        .find(|state| state["annotation"]["id"] == "legacy-note")
        .unwrap();
    assert!(legacy_note["annotation"].get("anchor").is_none());
    assert!(legacy_note["annotation"].get("title").is_none());
    assert_eq!(legacy_note["attachment"]["availability"], "anchorless");
    assert!(legacy_note["orphaned"].as_bool().unwrap());

    let legacy_view_edit = serde_json::json!({
        "id":"legacy-view","title":"Legacy edited","query":{"seed":seed}
    });
    let (status, legacy_view) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::PUT,
        &saved_pin_route("/api/views/legacy-view", &reopened_pin),
        Some(legacy_view_edit),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{legacy_view}");
    assert!(legacy_view["view"].get("anchor").is_none());
    assert_eq!(legacy_view["attachment"]["availability"], "anchorless");
    let legacy_note_edit = serde_json::json!({
        "id":"legacy-note","nodeId":seed,"body":"Legacy edited","title":"Legacy title"
    });
    let (status, legacy_note) = real_api(
        &client,
        &url,
        TOKEN,
        reqwest::Method::PUT,
        &saved_pin_route("/api/annotations/legacy-note", &reopened_pin),
        Some(legacy_note_edit),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "{legacy_note}");
    assert!(legacy_note["annotation"].get("anchor").is_none());
    assert_eq!(legacy_note["annotation"]["title"], "Legacy title");
    assert_eq!(legacy_note["attachment"]["availability"], "anchorless");
    assert!(raw_anchor(&stored_payload(&record_db, "views", "legacy-view")).is_none());
    assert!(raw_anchor(&stored_payload(&record_db, "annotations", "legacy-note")).is_none());
}

#[test]
fn optional_captured_scip_changes_presentation_without_native_identity_or_full_rewrite_breakage() {
    use sha2::{Digest, Sha256};
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("source");
    let home = temp.path().join("home");
    let ingress = temp.path().join("ingress");
    let other = temp.path().join("other");
    for dir in [&root, &ingress, &other] {
        fs::create_dir(dir).unwrap();
    }
    let source = "function f() {}\nf();\n";
    fs::write(root.join("main.js"), source).unwrap();
    let first = command(&root, &home, "index")
        .current_dir(&ingress)
        .arg("--scip")
        .arg("index.scip")
        .arg("--manifest")
        .arg("manifest.json")
        .output()
        .unwrap();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_pin: Value = serde_json::from_slice(&first.stdout).unwrap();
    // Startup reconciliation publishes r1. If its capture already used the
    // accepted request's SCIP options, the request is unchanged and completes
    // at r1 (Decision 0006 §3); otherwise it publishes r2 for the new inputs.
    assert!(
        (1..=2).contains(
            &first_pin["publishedRevision"]["indexRevision"]
                .as_u64()
                .unwrap()
        ),
        "{first_pin}"
    );
    let read_from_other = command(&root, &home, "status")
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(
        read_from_other.status.success(),
        "{}",
        String::from_utf8_lossy(&read_from_other.stderr)
    );
    let original = stopped_graph(&root, &home);
    let first_symbol = original["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "f")
        .unwrap();
    let first_id = first_symbol["id"].clone();
    assert!(first_symbol["displayLabel"].is_null());
    let hash = hex::encode(Sha256::digest(source.as_bytes()));
    write_optional_presentation(&ingress, "scip npm display 1 main.js/f().", &hash);
    // An unrelated cwd never becomes the base of the recorded optional inputs.
    write_optional_presentation(&other, "scip npm display 1 other().", "stale");
    let republished = command(&root, &home, "index")
        .arg("--scip")
        .arg(ingress.join("index.scip"))
        .arg("--manifest")
        .arg(ingress.join("manifest.json"))
        .output()
        .unwrap();
    assert!(
        republished.status.success(),
        "{}",
        String::from_utf8_lossy(&republished.stderr)
    );
    let republished: Value = serde_json::from_slice(&republished.stdout).unwrap();
    let read_from_other = command(&root, &home, "status")
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(
        read_from_other.status.success(),
        "{}",
        String::from_utf8_lossy(&read_from_other.stderr)
    );
    let changed = stopped_graph(&root, &home);
    let next = changed["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "f")
        .unwrap();
    assert_eq!(next["id"], first_id);
    assert_eq!(next["displayLabel"], "scip npm display 1 main.js/f().");
    assert_eq!(
        changed["files"][0]["text"].as_str().unwrap().as_bytes(),
        source.as_bytes()
    );
    let status = command(&root, &home, "status")
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(status.status.success());
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    // Read commands do not claim a writer. The daemon watcher can publish
    // another revision when selected presentation inputs change.
    assert!(
        status["revision"]["indexRevision"].as_u64().unwrap()
            >= republished["publishedRevision"]["indexRevision"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(
        status["revision"]["indexGeneration"],
        first_pin["publishedRevision"]["indexGeneration"]
    );
    write_optional_presentation(&ingress, "scip npm display 1 main.js/f().", "bad-hash");
    let republished = command(&root, &home, "index")
        .arg("--scip")
        .arg(ingress.join("index.scip"))
        .arg("--manifest")
        .arg(ingress.join("manifest.json"))
        .output()
        .unwrap();
    assert!(
        republished.status.success(),
        "{}",
        String::from_utf8_lossy(&republished.stderr)
    );
    let republished: Value = serde_json::from_slice(&republished.stdout).unwrap();
    let read_from_other = command(&root, &home, "status")
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(
        read_from_other.status.success(),
        "{}",
        String::from_utf8_lossy(&read_from_other.stderr)
    );
    let stale = stopped_graph(&root, &home);
    let symbol = stale["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == "f")
        .unwrap();
    assert_eq!(symbol["id"], first_id);
    assert!(symbol["displayLabel"].is_null());
    let latest = command(&root, &home, "status")
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(latest.status.success());
    let latest: Value = serde_json::from_slice(&latest.stdout).unwrap();
    assert_eq!(
        latest["revision"]["indexGeneration"],
        first_pin["publishedRevision"]["indexGeneration"]
    );
    assert!(
        latest["revision"]["indexRevision"].as_u64().unwrap()
            >= republished["publishedRevision"]["indexRevision"]
                .as_u64()
                .unwrap()
    );
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
