#![cfg(feature = "test-support")]
//! #842 (owner ruling 2026-11-09): a CLI read activates its checkout in the
//! daemon, as MCP and serve do, instead of failing `index_not_ready`.
use crate::cli::{FixtureDaemon, command, isolated_command, real_index_db};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

/// The CLI read budget a read had before #842; the first-head wait outlasts it.
const OLD_CLI_READ_WAIT: Duration = Duration::from_secs(5);

fn short_temp() -> tempfile::TempDir {
    // Keep Unix socket paths below the platform sockaddr_un bound.
    tempfile::Builder::new()
        .prefix("trellis-842-")
        .tempdir_in("/tmp")
        .unwrap()
}

/// A fresh Git checkout with one JavaScript file and no index.
fn fresh_checkout(base: &Path) -> PathBuf {
    let root = base.join("source");
    fs::create_dir_all(&root).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(init.status.success(), "git init failed");
    fs::write(root.join("flow.js"), "function firstSymbol() {}\n").unwrap();
    root
}

fn symbols(root: &Path, home: &Path, search: &str) -> Command {
    let mut read = command(root, home, "symbols");
    read.args(["--search", search, "--limit", "10"]);
    read
}

fn names(reply: &Value) -> Vec<String> {
    reply["data"]["items"]
        .as_array()
        .unwrap_or_else(|| panic!("no symbol items: {reply}"))
        .iter()
        .map(|item| item["name"].as_str().unwrap().to_owned())
        .collect()
}

/// Spawn a read whose output goes to files, so an auto-started daemon that
/// inherits its descriptors cannot hold a pipe open.
fn spawn_read(mut read: Command, dir: &Path, name: &str) -> (Child, PathBuf, PathBuf) {
    let stdout = dir.join(format!("{name}.stdout"));
    let stderr = dir.join(format!("{name}.stderr"));
    let child = read
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
        .spawn()
        .unwrap();
    (child, stdout, stderr)
}

fn accept(listener: &UnixListener, what: &str) -> UnixStream {
    listener.set_nonblocking(true).unwrap();
    let mut accepted = None;
    trellis::testing::wait_until(what, || match listener.accept() {
        Ok((stream, _)) => {
            accepted = Some(stream);
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(error) => panic!("accept {what}: {error}"),
    });
    let stream = accepted.unwrap();
    stream.set_nonblocking(false).unwrap();
    // macOS rejects the timeout once the peer has closed; its bytes stay readable.
    let _ = stream.set_read_timeout(Some(trellis::testing::HANG_BACKSTOP));
    stream
}

/// The daemon's startup reconciliation, held at its pre-capture hook.
fn held_startup(listener: &UnixListener) -> UnixStream {
    let mut startup = accept(listener, "startup reconciliation hook");
    startup.read_exact(&mut [0]).unwrap();
    startup
}

fn wait_exit(child: &mut Child, what: &str) -> std::process::ExitStatus {
    let mut status = None;
    trellis::testing::wait_until(what, || {
        status = child.try_wait().unwrap();
        status.is_some()
    });
    status.unwrap()
}

fn read_output(child: &mut Child, stdout: &Path, stderr: &Path, what: &str) -> Output {
    let status = wait_exit(child, what);
    Output {
        status,
        stdout: fs::read(stdout).unwrap(),
        stderr: fs::read(stderr).unwrap(),
    }
}

/// A fresh checkout: the read activates it, waits past the old read budget for
/// the first validated head, and answers. The daemon keeps watching it, so a
/// later edit reaches a later read without `trellis index`.
#[test]
fn fresh_checkout_read_activates_waits_for_the_first_head_and_keeps_watching() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fresh_checkout(temp.path());
    let _daemon = FixtureDaemon::new(&home);
    let startup_path = home.join("startup.sock");
    let wait_path = home.join("wait.sock");
    let startup_listener = UnixListener::bind(&startup_path).unwrap();
    let wait_listener = UnixListener::bind(&wait_path).unwrap();
    // The daemon this read starts watches with real OS delivery.
    let mut read = trellis::testing::with_native_watcher(symbols(&root, &home, "firstSymbol"));
    read.env("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path)
        .env("TRELLIS_TEST_CLI_READ_WAIT_SOCKET", &wait_path);
    let (mut child, stdout, stderr) = spawn_read(read, temp.path(), "first");
    let mut startup = held_startup(&startup_listener);
    let mut waiting = accept(&wait_listener, "first-head wait");
    let mut arrived = [0];
    waiting.read_exact(&mut arrived).unwrap();
    assert_eq!(arrived, *b"W");
    // Hold the first index past the read budget a read had before #842.
    std::thread::sleep(OLD_CLI_READ_WAIT + Duration::from_secs(1));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the read gave up before the first head: {}",
        fs::read_to_string(&stderr).unwrap()
    );
    startup.write_all(&[1]).unwrap();
    let first = read_output(&mut child, &stdout, &stderr, "first read");
    let first = crate::child::cli_json(&first, "first read");
    assert_eq!(names(&first), ["firstSymbol"], "{first}");
    let progress = fs::read_to_string(&stderr).unwrap();
    assert!(
        progress.contains("trellis: indexing this checkout"),
        "no progress on stderr: {progress}"
    );
    fs::write(
        root.join("flow.js"),
        "function firstSymbol() {}\nfunction laterSymbol() {}\n",
    )
    .unwrap();
    trellis::testing::wait_until("the watcher publishes the edit", || {
        let output = symbols(&root, &home, "laterSymbol").output().unwrap();
        let reply = crate::child::cli_json(&output, "later read");
        names(&reply).iter().any(|name| name == "laterSymbol")
    });
}

/// A prior valid head answers while startup reconciliation is held, reporting
/// `catchingUp:true` as MCP does.
#[test]
fn prior_head_answers_with_catching_up_while_reconciliation_is_pending() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fresh_checkout(temp.path());
    let daemon = FixtureDaemon::new(&home);
    let indexed = command(&root, &home, "index").output().unwrap();
    crate::child::assert_cli_success(&indexed, "indexed");
    daemon.stop_attested().unwrap();
    let startup_path = home.join("startup.sock");
    let startup_listener = UnixListener::bind(&startup_path).unwrap();
    let _replacement = FixtureDaemon::new(&home);
    let mut read = symbols(&root, &home, "firstSymbol");
    read.env("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path);
    let (mut child, stdout, stderr) = spawn_read(read, temp.path(), "prior");
    let mut startup = held_startup(&startup_listener);
    // The read answers while reconciliation is still held.
    let output = read_output(&mut child, &stdout, &stderr, "prior-head read");
    let reply = crate::child::cli_json(&output, "prior-head read");
    assert_eq!(reply["catchingUp"], true, "{reply}");
    assert_eq!(names(&reply), ["firstSymbol"], "{reply}");
    startup.write_all(&[1]).unwrap();
}

/// The bounded wait elapses while the first index is held: the read fails
/// typed `index_not_ready`, indexing continues, and a retry answers.
#[test]
fn elapsed_first_head_wait_is_typed_and_indexing_continues() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fresh_checkout(temp.path());
    let _daemon = FixtureDaemon::new(&home);
    let startup_path = home.join("startup.sock");
    let startup_listener = UnixListener::bind(&startup_path).unwrap();
    // The daemon this status starts inherits the shortened wait.
    let mut read = command(&root, &home, "status");
    read.env("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path)
        .env("TRELLIS_TEST_CLI_FIRST_INDEX_WAIT_MS", "1500");
    let (mut child, stdout, stderr) = spawn_read(read, temp.path(), "expired");
    let mut startup = held_startup(&startup_listener);
    let expired = read_output(&mut child, &stdout, &stderr, "expired read");
    assert_eq!(expired.status.code(), Some(1));
    assert!(expired.stdout.is_empty(), "partial output");
    let message = String::from_utf8_lossy(&expired.stderr);
    assert!(
        message.contains("trellis: indexing this checkout"),
        "no progress before expiry: {message}"
    );
    assert!(
        message.contains("Error: index_not_ready: ")
            && message.contains("indexing continues in the daemon, retry"),
        "untyped expiry: {message}"
    );
    // Released, the held first index completes with no further request.
    startup.write_all(&[1]).unwrap();
    let mut settle = isolated_command(&home);
    settle.env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"));
    let settled = crate::child::settled(settle, &root, None);
    assert_eq!(settled["indexRevision"], 1, "{settled}");
    let retried = command(&root, &home, "status").output().unwrap();
    let status = crate::child::cli_json(&retried, "retried status");
    assert_eq!(status["revision"], settled, "{status}");
}

/// An old-schema index is recreated by the activation, and the read answers.
#[test]
fn old_schema_index_is_recreated_and_the_read_answers() {
    use trellis::store::topology::UseGuard;
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fresh_checkout(temp.path());
    let daemon = FixtureDaemon::new(&home);
    let indexed = command(&root, &home, "index").output().unwrap();
    let indexed = crate::child::cli_json(&indexed, "indexed");
    daemon.stop_attested().unwrap();
    let dbpath = real_index_db(&home);
    let use_lock = dbpath.parent().unwrap().with_extension("lock");
    {
        let _exclusive = UseGuard::acquire_existing_readonly_exclusive(&use_lock)
            .expect("retired daemon retained the index-use lock");
        let db = rusqlite::Connection::open(&dbpath).unwrap();
        // An obsolete disposable index requires startup recreation.
        db.execute_batch(
            "PRAGMA ignore_check_constraints=ON;
             UPDATE index_metadata SET schema_version=7;",
        )
        .unwrap();
    }
    let _replacement = FixtureDaemon::new(&home);
    let output = symbols(&root, &home, "firstSymbol").output().unwrap();
    let reply = crate::child::cli_json(&output, "read after the old schema");
    assert_eq!(names(&reply), ["firstSymbol"], "{reply}");
    assert_ne!(
        reply["data"]["revision"]["indexGeneration"],
        indexed["publishedRevision"]["indexGeneration"],
        "the old schema was read through, not recreated: {reply}"
    );
}

/// A fresh checkout whose startup recovery blocks: the first-head wait ends
/// as soon as the blocker is published. `status` reports the
/// `recovery_blocked` cause, and other reads fail with the same typed blocker,
/// not `index_not_ready` after the full wait.
#[test]
fn blocked_startup_recovery_ends_the_first_head_wait_with_its_cause() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fresh_checkout(temp.path());
    let _daemon = FixtureDaemon::new(&home);
    // The daemon this status starts fails every startup attempt.
    let mut read = command(&root, &home, "status");
    read.env("TRELLIS_TEST_DAEMON_STARTUP_FAIL", "1");
    let started = std::time::Instant::now();
    let (mut child, stdout, stderr) = spawn_read(read, temp.path(), "blocked-status");
    let status = read_output(&mut child, &stdout, &stderr, "blocked status");
    let elapsed = started.elapsed();
    let message = String::from_utf8_lossy(&status.stderr);
    assert_eq!(status.status.code(), Some(1), "{message}");
    assert!(status.stdout.is_empty(), "partial output");
    assert!(
        message.contains("Error: recovery_blocked: "),
        "status hid the recovery blocker: {message}"
    );
    // The blocker publishes after five backed-off attempts, a few seconds;
    // the first-head wait is 90 s.
    assert!(
        elapsed < Duration::from_secs(60),
        "status waited {elapsed:?} for a blocked recovery"
    );
    let symbols = symbols(&root, &home, "firstSymbol").output().unwrap();
    let message = String::from_utf8_lossy(&symbols.stderr);
    assert_eq!(symbols.status.code(), Some(1), "{message}");
    assert!(symbols.stdout.is_empty(), "partial output");
    assert!(
        message.contains("Error: recovery_blocked: "),
        "symbols did not fail typed as blocked: {message}"
    );
}
