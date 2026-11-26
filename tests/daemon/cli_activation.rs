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

/// How a stopped daemon's index is left for the next daemon to recreate.
#[derive(Clone, Copy)]
enum Damage {
    /// The disposable index of an earlier release, as an upgrade leaves it.
    EarlierSchema,
    /// A corrupt file: real exceptional recovery.
    Corrupt,
}

/// Leave the index of the stopped daemon under `home` for startup to recreate.
fn damage_index(home: &Path, damage: Damage) {
    use trellis::store::topology::UseGuard;
    let dbpath = real_index_db(home);
    let use_lock = dbpath.parent().unwrap().with_extension("lock");
    let _exclusive = UseGuard::acquire_existing_readonly_exclusive(&use_lock)
        .expect("retired daemon retained the index-use lock");
    match damage {
        Damage::EarlierSchema => {
            let db = rusqlite::Connection::open(&dbpath).unwrap();
            // An obsolete disposable index requires startup recreation.
            db.execute_batch(
                "PRAGMA ignore_check_constraints=ON;
                 UPDATE index_metadata SET schema_version=7;",
            )
            .unwrap();
        }
        Damage::Corrupt => {
            use std::io::{Seek, SeekFrom};
            let mut file = fs::OpenOptions::new().write(true).open(&dbpath).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(b"not sqlite format").unwrap();
            file.sync_all().unwrap();
        }
    }
}

/// An old-schema index is recreated by the activation, and the read answers.
#[test]
fn old_schema_index_is_recreated_and_the_read_answers() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fresh_checkout(temp.path());
    let daemon = FixtureDaemon::new(&home);
    let indexed = command(&root, &home, "index").output().unwrap();
    let indexed = crate::child::cli_json(&indexed, "indexed");
    daemon.stop_attested().unwrap();
    damage_index(&home, Damage::EarlierSchema);
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
/// not `index_not_ready` after the full wait. (Before the fix, status waited
/// the whole first-head bound and then failed `index_not_ready`.)
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
    let (mut child, stdout, stderr) = spawn_read(read, temp.path(), "blocked-status");
    let status = read_output(&mut child, &stdout, &stderr, "blocked status");
    let message = String::from_utf8_lossy(&status.stderr);
    assert_eq!(status.status.code(), Some(1), "{message}");
    assert!(status.stdout.is_empty(), "partial output");
    assert!(
        message.contains("Error: recovery_blocked: "),
        "status hid the recovery blocker: {message}"
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

/// A prior valid head stays readable after startup recovery blocks: `symbols`
/// answers from it with `catchingUp:true`, while `status` reports the
/// `recovery_blocked` cause as it did before #842.
#[test]
fn prior_head_answers_after_startup_recovery_blocks() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fresh_checkout(temp.path());
    let daemon = FixtureDaemon::new(&home);
    let indexed = command(&root, &home, "index").output().unwrap();
    crate::child::assert_cli_success(&indexed, "indexed");
    daemon.stop_attested().unwrap();
    let _replacement = FixtureDaemon::new(&home);
    // The daemon the first status starts fails every startup attempt. Until
    // the fifth failure publishes the blocker, status answers from the head.
    let mut blocked = None;
    trellis::testing::wait_until("startup recovery blocks", || {
        let mut status = command(&root, &home, "status");
        status.env("TRELLIS_TEST_DAEMON_STARTUP_FAIL", "1");
        let output = status.output().unwrap();
        if output.status.success() {
            return false;
        }
        blocked = Some(output);
        true
    });
    let blocked = blocked.unwrap();
    let message = String::from_utf8_lossy(&blocked.stderr);
    assert!(blocked.stdout.is_empty(), "partial output");
    assert!(
        message.contains("Error: recovery_blocked: "),
        "status did not report the recovery blocker: {message}"
    );
    let output = symbols(&root, &home, "firstSymbol").output().unwrap();
    let reply = crate::child::cli_json(&output, "prior-head read after the block");
    assert_eq!(reply["catchingUp"], true, "{reply}");
    assert_eq!(names(&reply), ["firstSymbol"], "{reply}");
}

/// A Git checkout indexed by one daemon, which is then stopped and its index left `damage`d
/// for the next daemon's startup to recreate. Returns `(home, root)`.
fn indexed_then_damaged(temp: &Path, damage: Damage) -> (PathBuf, PathBuf) {
    let home = temp.join("home");
    fs::create_dir_all(&home).unwrap();
    let root = fresh_checkout(temp);
    fs::write(root.join("package.json"), "{\"name\":\"fixture\"}\n").unwrap();
    let daemon = FixtureDaemon::new(&home);
    let indexed = command(&root, &home, "index").output().unwrap();
    crate::child::assert_cli_success(&indexed, "indexed");
    daemon.stop_attested().unwrap();
    damage_index(&home, damage);
    (home, root)
}

/// `producer plan` for scip-typescript, with a fake install of its pinned version on `PATH`.
fn producer_plan(temp: &Path, home: &Path, root: &Path) -> Command {
    use crate::support::{fake_npm_install, pinned_version};
    let bin = fake_npm_install(
        &temp.join("ts"),
        "scip-typescript",
        &pinned_version("scip-typescript"),
    );
    let mut plan = isolated_command(home);
    plan.env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["producer", "plan", "--producer", "scip-typescript"])
        .args(["--trust", "restricted", "--workspace"])
        .arg(root);
    plan
}

/// `producer status`, which reads the head's native revision when there is one.
fn producer_status(home: &Path, root: &Path) -> Output {
    isolated_command(home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .args(["producer", "status", "--workspace"])
        .arg(root)
        .output()
        .unwrap()
}

/// One MCP `tools/call` through a `trellis mcp` relay to the running daemon.
fn mcp_call(home: &Path, root: &Path, name: &str, arguments: Value) -> Value {
    use std::io::BufRead;
    let mut child = isolated_command(home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .args(["mcp", "--workspace"])
        .arg(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            fs::File::create(home.join("mcp.stderr")).unwrap(),
        ))
        .spawn()
        .unwrap();
    let call = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
        "name":name,"arguments":arguments,
        "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientCapabilities":{}}}});
    writeln!(child.stdin.as_mut().unwrap(), "{call}").unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sent, reply) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = std::io::BufReader::new(stdout).read_line(&mut line);
        let _ = sent.send(line);
    });
    let line = reply
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("MCP reply");
    let _ = child.kill();
    let _ = child.wait();
    serde_json::from_str(&line).unwrap_or_else(|error| panic!("{error}: {line:?}"))
}

/// One browser route through `trellis api`: its exit code and answer.
fn api_get(home: &Path, target: &str) -> (i32, Value) {
    let output = isolated_command(home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .args(["api", "GET", target])
        .output()
        .unwrap();
    let value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "api {target} printed no JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    (output.status.code().unwrap(), value)
}

/// The browser status route of the only checkout under `home`.
fn status_route(home: &Path) -> String {
    let db = real_index_db(home);
    let key = db.parent().unwrap().file_name().unwrap().to_str().unwrap();
    format!("/api/checkouts/{key}/status")
}

/// #1324: while startup recreates the index an upgrade left behind, every surface answers the
/// ordinary not-ready a first index gives, with the upgrade as its reason, and never
/// `recovery_required`. Then the recreate completes and the same reads answer.
#[test]
fn recreate_after_upgrade_is_not_ready_on_cli_mcp_and_browser() {
    const REASON: &str = "index_not_ready: recreating index after upgrade";
    let temp = short_temp();
    let (home, root) = indexed_then_damaged(temp.path(), Damage::EarlierSchema);
    let startup_path = home.join("startup.sock");
    let startup_listener = UnixListener::bind(&startup_path).unwrap();
    let _replacement = FixtureDaemon::new(&home);
    // The daemon this status starts holds the recreate and waits briefly for a head.
    let mut read = command(&root, &home, "status");
    read.env("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path)
        .env("TRELLIS_TEST_CLI_FIRST_INDEX_WAIT_MS", "1500");
    let (mut child, stdout, stderr) = spawn_read(read, temp.path(), "recreating-status");
    let mut startup = held_startup(&startup_listener);
    let status = read_output(&mut child, &stdout, &stderr, "status while recreating");
    let message = String::from_utf8_lossy(&status.stderr);
    assert_eq!(status.status.code(), Some(1), "{message}");
    assert!(status.stdout.is_empty(), "partial output");
    assert!(
        message.contains("trellis: recreating index after upgrade")
            && message.contains(&format!("Error: {REASON}")),
        "status did not report the recreate: {message}"
    );

    // The CLI read the issue saw fail `recovery_required`.
    let plan = producer_plan(temp.path(), &home, &root).output().unwrap();
    let message = String::from_utf8_lossy(&plan.stderr);
    assert_eq!(plan.status.code(), Some(1), "{message}");
    assert!(
        message.contains(&format!("Error: {REASON}")) && !message.contains("recovery_required"),
        "producer plan did not report the recreate: {message}"
    );
    // With no head yet, producer status answers without one, as for a first index.
    let status = producer_status(&home, &root);
    let status = crate::child::cli_json(&status, "producer status while recreating");
    assert!(status["head"].is_null(), "{status}");

    let found = mcp_call(
        &home,
        &root,
        "trellis_find_symbols",
        serde_json::json!({"schemaVersion": 1, "query": "firstSymbol"}),
    );
    let error = &found["result"]["structuredContent"]["error"];
    assert_eq!(error["code"], "index_not_ready", "{found}");
    assert_eq!(error["retryable"], true, "{found}");

    let (code, answer) = api_get(&home, &status_route(&home));
    assert_eq!(code, 1, "{answer}");
    assert_eq!(answer["error"]["code"], "index_not_ready", "{answer}");

    // Released, the held recreate publishes a fresh index and the reads answer.
    startup.write_all(&[1]).unwrap();
    let mut settle = isolated_command(&home);
    settle.env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"));
    let settled = crate::child::settled(settle, &root, None);
    assert_eq!(settled["indexRevision"], 1, "{settled}");
    let (code, answer) = api_get(&home, &status_route(&home));
    assert_eq!(code, 0, "{answer}");
    assert_eq!(answer["revision"], settled, "{answer}");
    let plan = producer_plan(temp.path().join("after").as_path(), &home, &root)
        .output()
        .unwrap();
    let plan = crate::child::cli_json(&plan, "producer plan after the recreate");
    assert_eq!(plan["producer"]["name"], "scip-typescript", "{plan}");
}

/// #1324: `producer plan` waits for the recreate as the reads wait for a first head, past the
/// read budget it had, and answers once the recreate is published.
#[test]
fn producer_plan_waits_for_the_recreate_then_answers() {
    let temp = short_temp();
    let (home, root) = indexed_then_damaged(temp.path(), Damage::EarlierSchema);
    let startup_path = home.join("startup.sock");
    let wait_path = home.join("wait.sock");
    let startup_listener = UnixListener::bind(&startup_path).unwrap();
    let wait_listener = UnixListener::bind(&wait_path).unwrap();
    let _replacement = FixtureDaemon::new(&home);
    let mut plan = producer_plan(temp.path(), &home, &root);
    plan.env("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path)
        .env("TRELLIS_TEST_CLI_READ_WAIT_SOCKET", &wait_path);
    let (mut child, stdout, stderr) = spawn_read(plan, temp.path(), "waiting-plan");
    let mut startup = held_startup(&startup_listener);
    // Before #1324 the plan failed at once instead of waiting: report that, not a hang.
    wait_listener.set_nonblocking(true).unwrap();
    let mut accepted = None;
    trellis::testing::wait_until("producer plan waits for the recreate", || {
        if child.try_wait().unwrap().is_some() {
            panic!(
                "the plan ended instead of waiting: {}",
                fs::read_to_string(&stderr).unwrap()
            );
        }
        match wait_listener.accept() {
            Ok((stream, _)) => {
                accepted = Some(stream);
                true
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("accept the plan's wait: {error}"),
        }
    });
    let mut waiting = accepted.unwrap();
    waiting.set_nonblocking(false).unwrap();
    let mut arrived = [0];
    waiting.read_exact(&mut arrived).unwrap();
    assert_eq!(arrived, *b"W");
    // Hold the recreate past the read budget the plan had before #1324.
    std::thread::sleep(OLD_CLI_READ_WAIT + Duration::from_secs(1));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the plan gave up before the recreate: {}",
        fs::read_to_string(&stderr).unwrap()
    );
    startup.write_all(&[1]).unwrap();
    let output = read_output(&mut child, &stdout, &stderr, "waiting plan");
    let plan = crate::child::cli_json(&output, "producer plan");
    assert_eq!(plan["producer"]["name"], "scip-typescript", "{plan}");
    let progress = fs::read_to_string(&stderr).unwrap();
    assert!(
        progress.contains("trellis: recreating index after upgrade"),
        "no recreate progress on stderr: {progress}"
    );
}

/// #1324: a corrupt index awaiting its exceptional recreate is real recovery. It is never
/// reported as the upgrade's ordinary recreate, and `producer status`, which reads the store's
/// head, keeps `recovery_required`.
#[test]
fn corrupt_index_awaiting_recreate_keeps_recovery_required() {
    let temp = short_temp();
    let (home, root) = indexed_then_damaged(temp.path(), Damage::Corrupt);
    let startup_path = home.join("startup.sock");
    let startup_listener = UnixListener::bind(&startup_path).unwrap();
    let _replacement = FixtureDaemon::new(&home);
    let mut read = command(&root, &home, "status");
    read.env("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path)
        .env("TRELLIS_TEST_CLI_FIRST_INDEX_WAIT_MS", "1500");
    let (mut child, stdout, stderr) = spawn_read(read, temp.path(), "corrupt-status");
    let mut startup = held_startup(&startup_listener);
    let status = read_output(&mut child, &stdout, &stderr, "status while corrupt");
    let message = String::from_utf8_lossy(&status.stderr);
    assert_eq!(status.status.code(), Some(1), "{message}");
    assert!(
        !message.contains("recreating index after upgrade"),
        "corruption reported as an upgrade: {message}"
    );
    let status = producer_status(&home, &root);
    let message = String::from_utf8_lossy(&status.stderr);
    assert_eq!(status.status.code(), Some(1), "{message}");
    assert!(
        message.contains("Error: recovery_required: exceptional index recovery deferred"),
        "corruption lost recovery_required: {message}"
    );
    startup.write_all(&[1]).unwrap();
}
