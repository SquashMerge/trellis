#![cfg(feature = "test-support")]
//! Real executable coverage for the user-level socket and provisioned browser.
//! Selected-workspace HTTP routes remain covered separately by the legacy router;
//! this control-plane test does not assert that those routes are served by the daemon.
use serde_json::Value;
use std::{
    fs,
    net::{SocketAddr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::{Builder, TempDir};

fn short_temp() -> TempDir {
    // macOS Unix domain sockets have a 104-byte path limit. Long-HOME
    // socket-path behavior has its own regression test in the daemon slice.
    Builder::new().prefix("bg107-").tempdir_in("/tmp").unwrap()
}

/// Explicit platform parameter lets the Linux fixture rule run on macOS too.
fn fixture_roots_for(home: &Path, host: &str) -> trellis::store::topology::TopologyRoots {
    let (cache, data) = match host {
        "macos" => (
            home.join("Library/Caches/dev.squashmerge.trellis"),
            home.join("Library/Application Support/dev.squashmerge.trellis"),
        ),
        "linux" => (
            home.join(".cache/trellis"),
            home.join(".local/share/trellis"),
        ),
        other => panic!("unsupported fixture host: {other}"),
    };
    trellis::store::topology::TopologyRoots::isolated_for_tests(cache, data)
}
fn fixture_roots(home: &Path) -> trellis::store::topology::TopologyRoots {
    fixture_roots_for(home, std::env::consts::OS)
}
fn fixture_socket_paths(home: &Path) -> trellis::daemon::SocketPaths {
    trellis::daemon::SocketPaths::new(&fixture_roots(home).data)
}

#[test]
fn fixture_home_roots_match_linux_and_macos_layouts() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let linux = fixture_roots_for(&home, "linux");
    assert_eq!(linux.cache, home.join(".cache/trellis"));
    assert_eq!(linux.data, home.join(".local/share/trellis"));
    assert_ne!(
        linux.data,
        home.join("Library/Application Support/dev.squashmerge.trellis")
    );
    let mac = fixture_roots_for(&home, "macos");
    assert_eq!(
        mac.cache,
        home.join("Library/Caches/dev.squashmerge.trellis")
    );
    assert_eq!(
        mac.data,
        home.join("Library/Application Support/dev.squashmerge.trellis")
    );
    #[cfg(target_os = "linux")]
    {
        assert_eq!(fixture_roots(&home).data, linux.data);
        assert_eq!(
            fixture_socket_paths(&home).socket,
            linux.data.join("run/daemon.sock")
        );
        assert!(!mac.data.exists(), "wrong macOS fixture root was created");
    }
    #[cfg(target_os = "macos")]
    assert_eq!(fixture_roots(&home).data, mac.data);
}

#[test]
fn help_version_and_unstartable_daemon_never_open_standalone_store() {
    use std::os::unix::fs::PermissionsExt;

    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "unstartable");
    let paths = fixture_socket_paths(&home);
    let marker = home.join("auto-daemon.pid");
    for flag in ["--help", "--version"] {
        let output = cli(&home).arg(flag).output().unwrap();
        assert!(
            output.status.success(),
            "{flag}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty(), "{flag} printed no response");
        assert!(!paths.run.exists(), "{flag} started the daemon");
        assert!(!marker.exists(), "{flag} elected a daemon");
    }

    // A valid private HOME remains usable, but this ordinary file cannot be
    // replaced by SocketOwner. The CLI must fail closed, not index locally.
    fs::create_dir_all(&paths.run).unwrap();
    for dir in [&paths.run, paths.run.parent().unwrap()] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(paths.socket.parent(), Some(paths.run.as_path()));
    fs::write(&paths.socket, b"not a socket").unwrap();
    let started = Instant::now();
    let output = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        started.elapsed() < trellis::testing::HANG_BACKSTOP,
        "daemon start was unbounded"
    );
    assert!(!output.status.success(), "index unexpectedly succeeded");
    assert!(output.stdout.is_empty(), "index emitted partial output");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("daemon_unavailable"),
        "untyped daemon failure: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read(&paths.socket).unwrap(), b"not a socket");
    assert!(!marker.exists(), "failed daemon claimed ownership");
    assert!(
        !fixture_roots(&home).cache.join("indexes").exists(),
        "unstartable daemon fell back to a standalone index"
    );
}

fn bounded_fixture_stderr(path: &Path) -> String {
    use std::io::Read;
    let Ok(file) = fs::File::open(path) else {
        return "<stderr unavailable>".to_owned();
    };
    let mut bytes = Vec::new();
    let _ = file.take(8192).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

fn fixture_daemon_diagnostic(home: &Path, expected: &Path) -> String {
    use std::os::unix::fs::FileTypeExt;
    let candidates = [
        fixture_socket_paths(home).socket,
        trellis::daemon::SocketPaths::new(&fixture_roots_for(home, "macos").data).socket,
        trellis::daemon::SocketPaths::new(&fixture_roots_for(home, "linux").data).socket,
    ];
    let observed: Vec<_> = candidates
        .iter()
        .filter(|path| fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket()))
        .collect();
    let mut stderr = Vec::new();
    if let Ok(entries) = fs::read_dir(home) {
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("daemon-cli-child-")
            {
                continue;
            }
            stderr.push(format!(
                "{}: {}",
                entry.path().display(),
                bounded_fixture_stderr(&entry.path())
            ));
            if stderr.len() == 8 {
                break;
            }
        }
    }
    format!(
        "expected socket={}; actual socket candidates={observed:?}; bounded daemon stderr={stderr:?}",
        expected.display()
    )
}

fn cli(home: &Path) -> Command {
    let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    command
        .env("HOME", home)
        .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null());
    command
}

/// Cold CLI fixture owns the elected daemon even after its launching CLI exits.
struct AutoDaemonGuard(std::path::PathBuf);
impl AutoDaemonGuard {
    fn for_home(home: &Path) -> Self {
        Self(home.join("auto-daemon.pid"))
    }
}
impl Drop for AutoDaemonGuard {
    fn drop(&mut self) {
        if let Ok(pid) = fs::read_to_string(&self.0)
            && let Ok(pid) = pid.parse::<u32>()
            && pid <= i32::MAX as u32
        {
            // The private test HOME and PID marker are created by this fixture.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
}

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn detached(command: Command) -> Owned {
    detached_with_log(command).0
}
fn detached_with_log(mut command: Command) -> (Owned, std::path::PathBuf) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_LOG: AtomicU64 = AtomicU64::new(1);
    let home = command
        .get_envs()
        .find(|(key, _)| *key == std::ffi::OsStr::new("HOME"))
        .and_then(|(_, value)| value)
        .map(std::path::PathBuf::from)
        .expect("fixture child has a private HOME");
    let log = home.join(format!(
        "daemon-cli-child-{}-{}.stderr",
        std::process::id(),
        NEXT_LOG.fetch_add(1, Ordering::Relaxed)
    ));
    let child = command
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(&log).unwrap()))
        .spawn()
        .unwrap();
    (Owned(child), log)
}
fn free_address() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}
fn checkout(base: &Path, name: &str) -> std::path::PathBuf {
    let root = base.join(name);
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("sample.js"),
        format!("function {name}Symbol() {{ return 1; }}\n"),
    )
    .unwrap();
    root
}
fn serve(home: &Path, root: &Path, bind: SocketAddr, token: &Path) -> Command {
    let mut command = cli(home);
    command
        .arg("serve")
        .arg("--workspace")
        .arg(root)
        .arg("--bind")
        .arg(bind.to_string())
        .arg("--token-file")
        .arg(token);
    command
}
fn run(home: &Path, command: &str, root: &Path) -> Value {
    let output = cli(home)
        .arg(command)
        .arg("--workspace")
        .arg(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{command} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
async fn serve_banner_address(child: &mut Child, log: &Path, home: &Path) -> SocketAddr {
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        let stderr = bounded_fixture_stderr(log);
        if let Some(raw) = stderr
            .lines()
            .find_map(|line| line.strip_prefix("Trellis: http://")?.strip_suffix('/'))
        {
            let address: SocketAddr = raw
                .parse()
                .expect("serve banner must contain a socket address");
            assert!(address.ip().is_loopback() && address.port() != 0);
            return address;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "serve exited before address banner: {}",
            fixture_daemon_diagnostic(home, &fixture_socket_paths(home).socket)
        );
        assert!(
            Instant::now() < deadline,
            "serve address banner timed out: {}",
            fixture_daemon_diagnostic(home, &fixture_socket_paths(home).socket)
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

async fn ready(client: &reqwest::Client, address: SocketAddr, child: &mut Child, home: &Path) {
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if let Ok(reply) = client.get(format!("http://{address}/healthz")).send().await
            && reply.status().is_success()
        {
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "serve process exited before /healthz: {}",
            fixture_daemon_diagnostic(home, &fixture_socket_paths(home).socket)
        );
        assert!(
            Instant::now() < deadline,
            "serve did not provision /healthz"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
}

#[test]
fn serve_request_preserves_explicit_default_file_cap_presence() {
    use std::sync::mpsc;
    use trellis::daemon::{SocketOwner, protocol};
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "request-options");
    let owner = SocketOwner::acquire(&fixture_socket_paths(&home))
        .unwrap()
        .unwrap();
    let listener = owner.listener().try_clone().unwrap();
    let (tx, rx) = mpsc::sync_channel(2);
    let responder = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let request: protocol::Request = protocol::read_frame(&mut stream).unwrap();
            assert_eq!(request.operation, "serve");
            tx.send(request.payload["options"].clone()).unwrap();
            protocol::write_frame(
                &mut stream,
                &protocol::Reply {
                    id: request.id,
                    payload: serde_json::json!({"result":"127.0.0.1:8877"}),
                },
            )
            .unwrap();
        }
    });
    let mut options = Vec::new();
    for explicit in [false, true] {
        let mut command = cli(&home);
        command
            .arg("serve")
            .arg("--workspace")
            .arg(&root)
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--token-file")
            .arg(home.join("token"));
        if explicit {
            command.arg("--max-file-bytes").arg("2097152");
        }
        // A wrong fake socket must fail within this fixture deadline, not wait
        // for the real daemon's 30-minute idle exit after an accidental start.
        let stderr_path = home.join(format!("fake-serve-{explicit}.stderr"));
        let mut child = Owned(
            command
                .stdout(Stdio::null())
                .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            if child.0.try_wait().unwrap().is_some() {
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.0.kill();
                let _ = child.0.wait();
                panic!(
                    "fake serve acknowledgement timed out: {}; {}",
                    bounded_fixture_stderr(&stderr_path),
                    fixture_daemon_diagnostic(&home, &fixture_socket_paths(&home).socket)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let stderr = bounded_fixture_stderr(&stderr_path);
        assert!(
            stderr.contains("Trellis:"),
            "serve did not receive fake acknowledgement: {stderr}; {}",
            fixture_daemon_diagnostic(&home, &fixture_socket_paths(&home).socket)
        );
        options.push(
            rx.recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap_or_else(|error| {
                    panic!(
                        "fake responder did not receive serve request: {error}; {stderr}; {}",
                        fixture_daemon_diagnostic(&home, &fixture_socket_paths(&home).socket)
                    )
                }),
        );
    }
    responder.join().unwrap();
    assert!(
        options[0].get("maxFileBytes").is_none(),
        "omission became explicit: {}",
        options[0]
    );
    assert!(
        options[0].get("scip").is_none(),
        "omitted SCIP became explicit: {}",
        options[0]
    );
    assert!(
        options[0].get("manifest").is_none(),
        "omitted manifest became explicit: {}",
        options[0]
    );
    assert_eq!(options[1]["maxFileBytes"], 2_097_152, "{}", options[1]);
    assert!(options[1].get("scip").is_none(), "{}", options[1]);
    assert!(options[1].get("manifest").is_none(), "{}", options[1]);
    drop(owner);
}

// Socket acceptance does not imply the virgin checkout's first publication.
fn status_when_ready(
    mut read: impl FnMut() -> std::process::Output,
    on_first_not_ready: impl FnOnce(),
) -> Value {
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut on_first_not_ready = Some(on_first_not_ready);
    let mut attempts = 0;
    loop {
        attempts += 1;
        let output = read();
        if output.status.success() {
            return serde_json::from_slice(&output.stdout).unwrap();
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.stdout.is_empty(),
            "failed status emitted partial output"
        );
        assert_eq!(
            stderr.trim(),
            "Error: index_not_ready: reindex required",
            "unexpected status failure on attempt {attempts}: {stderr}",
        );
        if let Some(release) = on_first_not_ready.take() {
            release();
        }
        assert!(
            Instant::now() < deadline,
            "status did not become ready after {attempts} attempts: {stderr}",
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn status_readiness_does_not_retry_other_failures() {
    use std::os::unix::process::ExitStatusExt;
    let mut attempts = 0;
    let mut retry_observed = false;
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        status_when_ready(
            || {
                attempts += 1;
                std::process::Output {
                    status: std::process::ExitStatus::from_raw(1 << 8),
                    stdout: Vec::new(),
                    stderr: b"Error: daemon_unavailable\n".to_vec(),
                }
            },
            || retry_observed = true,
        );
    }));
    assert!(failure.is_err());
    assert_eq!(attempts, 1, "unrelated failure was retried");
    assert!(
        !retry_observed,
        "unrelated failure entered the readiness retry path"
    );
}

#[test]
fn daemon_keeps_private_socket_after_transient_accept_errors() {
    use std::os::unix::{
        fs::MetadataExt,
        net::{UnixListener, UnixStream},
    };
    use std::{
        io::{Read, Write},
        sync::mpsc,
    };
    for fault in ["emfile", "econnaborted"] {
        let temp = short_temp();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let root = checkout(temp.path(), "work");
        let startup_path = home.join("startup.sock");
        let startup_listener = UnixListener::bind(&startup_path).unwrap();
        startup_listener.set_nonblocking(true).unwrap();
        let (arrived, startup) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let mut accepted = None;
            trellis::testing::wait_until("daemon startup fixture connection", || {
                match startup_listener.accept() {
                    Ok((stream, _)) => {
                        accepted = Some(stream);
                        true
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
                    Err(error) => panic!("accept daemon startup fixture: {error}"),
                }
            });
            let mut stream = accepted.unwrap();
            stream
                .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
                .unwrap();
            stream.read_exact(&mut [0]).unwrap();
            // A failed status assertion may already have dropped the receiver.
            let _ = arrived.send(stream);
        });
        let mut command = cli(&home);
        command
            .arg("daemon")
            .env("TRELLIS_TEST_DAEMON_ACCEPT_ERROR_ONCE", fault)
            .env("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path);
        let mut owner = detached(command);
        let socket = fixture_socket_paths(&home).socket;
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "daemon exited after {fault}: {}",
                fixture_daemon_diagnostic(&home, &socket)
            );
            if UnixStream::connect(&socket).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "daemon did not accept after {fault}: {}",
                fixture_daemon_diagnostic(&home, &socket)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let inode = fs::metadata(&socket).unwrap().ino();
        let mut first_refusal_observed = false;
        for _ in 0..2 {
            let reply = status_when_ready(
                || {
                    cli(&home)
                        .arg("status")
                        .arg("--workspace")
                        .arg(&root)
                        .output()
                        .unwrap()
                },
                || {
                    let mut stream = startup
                        .recv_timeout(trellis::testing::HANG_BACKSTOP)
                        .expect("startup did not reach the pre-publication hook");
                    stream.write_all(&[1]).unwrap();
                    first_refusal_observed = true;
                },
            );
            assert!(reply.is_object(), "{reply}");
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "daemon died after {fault}"
            );
            assert_eq!(fs::metadata(&socket).unwrap().ino(), inode);
        }
        assert!(
            first_refusal_observed,
            "startup fixture never forced a cold refusal"
        );
        waiter.join().unwrap();
    }
}

#[tokio::test]
async fn executable_daemon_is_socket_only_until_explicit_serve_and_registers_two_checkouts() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let first = checkout(temp.path(), "first");
    let second = checkout(temp.path(), "second");
    let requested_bind: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let token_file = temp.path().join("token");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let mut daemon_command = cli(&home);
    daemon_command.arg("daemon");
    let mut daemon = detached(daemon_command);
    let socket = fixture_socket_paths(&home).socket;
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    // The socket file appears at bind(), just before listen(): wait for a
    // connection, not for the file.
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited before socket bind: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        assert!(
            Instant::now() < deadline,
            "daemon socket was not listening: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    // A socket-only daemon must not provision a browser token or listener.
    assert!(daemon.0.try_wait().unwrap().is_none());
    assert!(
        !token_file.exists(),
        "socket-only startup provisioned a browser token"
    );
    let duplicate = cli(&home).arg("daemon").output().unwrap();
    assert!(duplicate.status.success(), "second daemon election failed");
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "second daemon displaced socket owner"
    );
    assert!(!token_file.exists());

    // Let the daemon own port selection. Dropping a reserved free port before
    // serve lets another parallel test bind it first.
    let (mut first_serve, first_log) =
        detached_with_log(serve(&home, &first, requested_bind, &token_file));
    let address = serve_banner_address(&mut first_serve.0, &first_log, &home).await;
    ready(&client, address, &mut first_serve.0, &home).await;
    let token = fs::read_to_string(&token_file).unwrap();
    let url = format!("http://{address}/api/checkouts");
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("wrong-token")
            .send()
            .await
            .unwrap()
            .status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
    let authenticated = || client.get(&url).bearer_auth(token.trim());
    let response = authenticated().send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let entries = response.json::<Value>().await.unwrap();
    assert_eq!(entries["checkouts"].as_array().unwrap().len(), 1);
    assert_eq!(
        entries["checkouts"][0]["workspaceRoot"],
        fs::canonicalize(&first).unwrap().to_string_lossy().as_ref()
    );
    let status = client
        .get(format!("http://{address}/api/daemon/status"))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), reqwest::StatusCode::OK);
    assert_eq!(status.json::<Value>().await.unwrap()["activeCheckouts"], 0);
    let first_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&first), &first)
        .unwrap()
        .root_key;
    for (method, path) in [
        (reqwest::Method::GET, "/api/status"),
        (reqwest::Method::GET, "/api/dependencies"),
        (reqwest::Method::POST, "/api/index"),
    ] {
        let response = client
            .request(method.clone(), format!("http://{address}{path}"))
            .bearer_auth(token.trim())
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND,
            "{method} {path}"
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert!(!response.headers().contains_key("X-Trellis-Workspace"));
        assert!(!response.headers().contains_key("X-Trellis-Catching-Up"));
        let state = client
            .get(format!("http://{address}/api/daemon/status"))
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap();
        assert_eq!(
            state["activeCheckouts"], 0,
            "{method} {path} activated the checkout"
        );
    }
    let selected = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let response = client
                .get(format!("http://{address}/api/checkouts/{first_key}/status"))
                .bearer_auth(token.trim())
                .send()
                .await
                .unwrap();
            if response.status() == reqwest::StatusCode::OK {
                break response;
            }
            assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        selected.headers()["X-Trellis-Workspace"],
        fs::canonicalize(&first).unwrap().to_str().unwrap()
    );
    assert!(selected.headers().contains_key("X-Trellis-Catching-Up"));
    let selected: Value = selected.json().await.unwrap();
    assert_eq!(
        selected["workspaceRoot"],
        fs::canonicalize(&first).unwrap().to_str().unwrap()
    );
    // The current-job read is typed contention while startup reconciliation or the watcher writes;
    // retry only storage_busy and report any other refusal with its body.
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let jobs = loop {
        let jobs = client
            .get(format!(
                "http://{address}/api/checkouts/{first_key}/jobs/current"
            ))
            .bearer_auth(token.trim())
            .send()
            .await
            .unwrap();
        if jobs.status() != reqwest::StatusCode::CONFLICT || Instant::now() >= deadline {
            break jobs;
        }
        let body: Value = jobs.json().await.unwrap();
        assert_eq!(
            body["error"]["code"], "storage_busy",
            "current job refused: {body}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let status = jobs.status();
    let body = jobs.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "current job: {body}");
    assert!(
        serde_json::from_str::<Value>(&body).unwrap().is_null(),
        "old POST accepted a job"
    );
    let wrong_origin = authenticated()
        .header("Origin", "http://not-local.example")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_origin.status(), reqwest::StatusCode::FORBIDDEN);

    let output = serve(&home, &second, address, &token_file)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bind conflicts"));
    let other_token = temp.path().join("other-token");
    let output = serve(&home, &second, requested_bind, &other_token)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("token file conflicts"));
    assert!(!other_token.exists());
    assert_eq!(
        authenticated()
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["checkouts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let mut second_serve = detached(serve(&home, &second, requested_bind, &token_file));
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        // A listing can exceed the 2 s client timeout while the second
        // registration is busy under load; poll again until the deadline (#131).
        let value = match authenticated().send().await {
            Ok(response) => response.json::<Value>().await.unwrap(),
            Err(error) if error.is_timeout() => Value::Null,
            Err(error) => panic!("checkout listing failed: {error}"),
        };
        if value["checkouts"]
            .as_array()
            .is_some_and(|rows| rows.len() == 2)
        {
            let roots: Vec<_> = value["checkouts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["workspaceRoot"].as_str().unwrap().to_owned())
                .collect();
            assert!(
                roots.contains(
                    &fs::canonicalize(&first)
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                )
            );
            assert!(
                roots.contains(
                    &fs::canonicalize(&second)
                        .unwrap()
                        .to_string_lossy()
                        .to_string()
                )
            );
            break;
        }
        assert!(
            second_serve.0.try_wait().unwrap().is_none(),
            "second registration exited"
        );
        assert!(Instant::now() < deadline, "second checkout not registered");
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let (mut same_serve, same_log) =
        detached_with_log(serve(&home, &first, requested_bind, &token_file));
    let same_address = serve_banner_address(&mut same_serve.0, &same_log, &home).await;
    assert_eq!(
        same_address, address,
        "same registration changed the listener"
    );
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if same_serve.0.try_wait().unwrap().is_none() {
            // The same registration keeps the same singleton listener and checkout set.
            let list = authenticated()
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap();
            if list["checkouts"].as_array().unwrap().len() == 2 {
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "same registration did not stay attached"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&first)
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "socket index failed: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert!(
        run(&home, "status", &first)["revision"]["indexRevision"]
            .as_u64()
            .unwrap()
            >= 1
    );
    let symbols = cli(&home)
        .arg("symbols")
        .arg("--workspace")
        .arg(&first)
        .arg("--search")
        .arg("firstSymbol")
        .output()
        .unwrap();
    assert!(
        symbols.status.success(),
        "socket symbols failed: {}",
        String::from_utf8_lossy(&symbols.stderr)
    );
    assert!(
        !serde_json::from_slice::<Value>(&symbols.stdout).unwrap()["data"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Index activated this checkout. A conflicting registration cannot replace
    // its options until the active checkout is released.
    let mut conflict = serve(&home, &first, requested_bind, &token_file)
        .arg("--max-file-bytes")
        .arg("3145728")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if let Some(status) = conflict.try_wait().unwrap() {
            assert!(
                !status.success(),
                "active checkout accepted conflicting options"
            );
            let output = conflict.wait_with_output().unwrap();
            assert!(String::from_utf8_lossy(&output.stderr).contains("registration_conflict"));
            break;
        }
        if Instant::now() >= deadline {
            let _ = conflict.kill();
            let _ = conflict.wait();
            panic!("conflicting registration did not return within deadline");
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(first_serve.0.try_wait().unwrap().is_none());
    assert!(second_serve.0.try_wait().unwrap().is_none());
    assert!(daemon.0.try_wait().unwrap().is_none());
}

#[test]
fn cold_cli_commands_start_and_reuse_daemon() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "fallback");
    let output = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index daemon start failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = run(&home, "status", &root);
    assert!(status["revision"]["indexRevision"].as_u64().unwrap() >= 1);
    // Daemon status carries the watcher-health witness (Decision 0009 §5),
    // native once the current watcher's setup publishes.
    let until = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut status = status;
    while status["watcher"]["native"] != true {
        assert!(Instant::now() < until, "no native watcher: {status}");
        std::thread::sleep(Duration::from_millis(20));
        status = run(&home, "status", &root);
    }
    let witness = &status["watcher"];
    assert!(witness["epoch"].as_u64().unwrap() >= 1, "{status}");
    assert_eq!(witness["degraded"], false, "{status}");
    assert_eq!(witness["degradations"], 0, "{status}");
    let symbols = cli(&home)
        .arg("symbols")
        .arg("--workspace")
        .arg(&root)
        .arg("--search")
        .arg("fallbackSymbol")
        .output()
        .unwrap();
    assert!(
        symbols.status.success(),
        "symbols daemon call failed: {}",
        String::from_utf8_lossy(&symbols.stderr)
    );
    let symbols: Value = serde_json::from_slice(&symbols.stdout).unwrap();
    assert!(!symbols["data"]["items"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn long_home_socket_names_are_private_distinct_connectable_and_recover_after_crash() {
    use std::os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::UnixStream,
    };
    let temp = TempDir::new().unwrap();
    let long = "long-home-component-".repeat(7);
    let homes: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|label| temp.path().join(label).join(&long))
        .collect();
    let paths: Vec<_> = homes
        .iter()
        .map(|home| fixture_socket_paths(home))
        .collect();
    assert_ne!(
        paths[0].socket, paths[1].socket,
        "long HOME identities must not collide"
    );
    for paths in &paths {
        assert!(
            paths.socket.starts_with("/tmp"),
            "long HOME should use bounded private socket path"
        );
        assert!(paths.socket.as_os_str().as_encoded_bytes().len() <= 103);
    }
    let mut owners = Vec::new();
    for (home, paths) in homes.iter().zip(&paths) {
        fs::create_dir_all(home).unwrap();
        let mut command = cli(home);
        command.arg("daemon");
        let mut owner = detached(command);
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            if UnixStream::connect(&paths.socket).is_ok() {
                break;
            }
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "long-HOME daemon exited before accepting socket clients: {}",
                fixture_daemon_diagnostic(home, &paths.socket)
            );
            assert!(
                Instant::now() < deadline,
                "long-HOME daemon did not bind socket: {}",
                fixture_daemon_diagnostic(home, &paths.socket)
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let metadata = fs::symlink_metadata(&paths.socket).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            fs::metadata(paths.socket.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert!(owner.0.try_wait().unwrap().is_none());
        owners.push(owner);
    }
    assert!(UnixStream::connect(&paths[0].socket).is_ok());
    assert!(UnixStream::connect(&paths[1].socket).is_ok());
    // SIGKILL does not run SocketOwner::drop; election must recover the stale socket.
    use std::os::unix::fs::MetadataExt;
    let old_metadata = fs::symlink_metadata(&paths[0].socket).unwrap();
    let old_identity = (old_metadata.dev(), old_metadata.ino());
    owners[0].0.kill().unwrap();
    owners[0].0.wait().unwrap();
    let stale_metadata = fs::symlink_metadata(&paths[0].socket).unwrap();
    assert!(stale_metadata.file_type().is_socket());
    assert_eq!((stale_metadata.dev(), stale_metadata.ino()), old_identity);
    assert!(
        UnixStream::connect(&paths[0].socket).is_err(),
        "killed daemon's stale socket still accepts connections"
    );
    let proof_root = temp.path().join("recovered-socket-proof");
    fs::create_dir(&proof_root).unwrap();
    let pid_file = homes[0].join("recovered-daemon.pid");
    let mut command = cli(&homes[0]);
    command
        .arg("daemon")
        .env("TRELLIS_TEST_DAEMON_PID_FILE", &pid_file);
    let mut recovered = detached(command);
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    assert!(recovered.0.try_wait().unwrap().is_none());
    let mut recovered_socket = loop {
        if let Ok(metadata) = fs::symlink_metadata(&paths[0].socket)
            && metadata.file_type().is_socket()
            && metadata.permissions().mode() & 0o777 == 0o600
            && fs::metadata(paths[0].socket.parent().unwrap())
                .is_ok_and(|parent| parent.permissions().mode() & 0o777 == 0o700)
            && let Ok(stream) = UnixStream::connect(&paths[0].socket)
        {
            break stream;
        }
        let recovered_status = recovered.0.try_wait().unwrap();
        assert!(
            recovered_status.is_none(),
            "crash recovery daemon pid={} exited: {recovered_status:?}; {}",
            recovered.0.id(),
            fixture_daemon_diagnostic(&homes[0], &paths[0].socket)
        );
        if Instant::now() >= deadline {
            let current = fs::symlink_metadata(&paths[0].socket);
            let current_detail = current
                .as_ref()
                .map(|metadata| {
                    format!(
                        "dev={}, ino={}, socket={}, mode={:o}",
                        metadata.dev(),
                        metadata.ino(),
                        metadata.file_type().is_socket(),
                        metadata.mode() & 0o777
                    )
                })
                .unwrap_or_else(|error| format!("metadata error: {error}"));
            let parent_mode = fs::metadata(paths[0].socket.parent().unwrap())
                .map(|metadata| metadata.permissions().mode() & 0o777);
            let independent_connect = UnixStream::connect(&paths[0].socket)
                .map(|_| ())
                .map_err(|error| error.to_string());
            panic!(
                "stale long-HOME socket not recovered: old (dev, ino)={old_identity:?}, current={current_detail}, parent mode={parent_mode:?}, independent connect={independent_connect:?}, recovered pid={}, status={recovered_status:?}; {}",
                recovered.0.id(),
                fixture_daemon_diagnostic(&homes[0], &paths[0].socket)
            );
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    };
    // A reused inode is not an endpoint identity; prove this owned child serves
    // a framed request without attaching a checkout or writing its marker.
    assert!(
        recovered.0.try_wait().unwrap().is_none(),
        "recovered daemon exited before protocol proof: {}",
        fixture_daemon_diagnostic(&homes[0], &paths[0].socket)
    );
    use trellis::daemon::protocol;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .expect("recovered socket connected after the original 10-second deadline");
    recovered_socket.set_read_timeout(Some(remaining)).unwrap();
    recovered_socket.set_write_timeout(Some(remaining)).unwrap();
    let proof_id = 0x107_5a1e_u64;
    protocol::write_frame(
        &mut recovered_socket,
        &protocol::Request {
            id: proof_id,
            operation: "socket-recovery-proof".into(),
            payload: serde_json::json!({"workspace": proof_root}),
        },
    )
    .unwrap_or_else(|error| {
        panic!(
            "recovered socket protocol write failed: {error}; {}",
            fixture_daemon_diagnostic(&homes[0], &paths[0].socket)
        )
    });
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .expect("recovered socket write exceeded the original 10-second deadline");
    recovered_socket.set_read_timeout(Some(remaining)).unwrap();
    let reply: protocol::Reply =
        protocol::read_frame(&mut recovered_socket).unwrap_or_else(|error| {
            panic!(
                "recovered socket protocol read failed: {error}; {}",
                fixture_daemon_diagnostic(&homes[0], &paths[0].socket)
            )
        });
    assert!(
        Instant::now() < deadline,
        "recovered socket reply exceeded the original 10-second deadline"
    );
    assert_eq!(reply.id, proof_id);
    assert_eq!(
        reply.payload,
        serde_json::json!({"error": "unknown daemon operation"})
    );
    assert_eq!(
        fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap(),
        recovered.0.id(),
        "socket response came from a different elected daemon"
    );
    assert!(
        recovered.0.try_wait().unwrap().is_none(),
        "recovered daemon exited after protocol reply: {}",
        fixture_daemon_diagnostic(&homes[0], &paths[0].socket)
    );
    assert!(
        UnixStream::connect(&paths[1].socket).is_ok(),
        "unrelated HOME was displaced"
    );
}

#[test]
fn concurrent_cold_cli_indexes_share_one_daemon_and_keep_fifo_results() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "concurrent");
    let marker = home.join("elected.pid");
    // Kept so an intermittent no-reply hang can be diagnosed from the log.
    let daemon_log = home.join("daemon.stderr");
    let start = || {
        cli(&home)
            .arg("index")
            .arg("--workspace")
            .arg(&root)
            .env("TRELLIS_TEST_DAEMON_PID_FILE", &marker)
            .env("TRELLIS_INDEX_DIAGNOSTICS", "1")
            .env("TRELLIS_TEST_DAEMON_STDERR", &daemon_log)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let first = start();
    let second = start();
    for child in [first, second] {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}\ndaemon diagnostics:\n{}",
            String::from_utf8_lossy(&output.stderr),
            fs::read_to_string(&daemon_log).unwrap_or_default()
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        // status is the checkout's current head, sampled after this request
        // published. The other concurrent request (FIFO-after) may already
        // have published too, so status is at or after this revision.
        let published = &response["publishedRevision"];
        let current = &response["status"]["revision"];
        assert_eq!(
            current["indexGeneration"], published["indexGeneration"],
            "{response}"
        );
        assert!(
            current["indexRevision"].as_u64().unwrap()
                >= published["indexRevision"].as_u64().unwrap(),
            "{response}"
        );
    }
    let elected: u32 = fs::read_to_string(&marker).unwrap().parse().unwrap();
    assert!(elected > 0);
    assert!(std::os::unix::net::UnixStream::connect(fixture_socket_paths(&home).socket).is_ok());
    let roots = fixture_roots(&home);
    let identity =
        trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let queue = roots
        .cache
        .join("indexes")
        .join(&identity.root_key)
        .join("requests.db");
    let db =
        rusqlite::Connection::open_with_flags(queue, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let rows: Vec<(i64, String, String, i64)> = db
        .prepare("SELECT seq,id,state,result_revision FROM requests ORDER BY seq")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(rows.len(), 2);
    assert!(rows[0].0 < rows[1].0);
    assert!(rows.iter().all(|(_, _, state, _)| state == "done"));
    // Unchanged requests complete at the same head (Decision 0006 §3).
    assert_eq!(rows[0].3, rows[1].3);
    // FIFO is witnessed by the daemon worker's own claim and terminal markers,
    // emitted in order on its thread after each COMMIT: the second row is
    // claimed only after the first row's terminal commit.
    let expected = vec![
        format!("request_claimed id={}", rows[0].1),
        format!("request_terminal id={}", rows[0].1),
        format!("request_claimed id={}", rows[1].1),
        format!("request_terminal id={}", rows[1].1),
    ];
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let markers = loop {
        let log = fs::read_to_string(&daemon_log).unwrap_or_default();
        let lines: Vec<&str> = log
            .lines()
            .filter(|line| {
                line.contains("event=request_claimed ") || line.contains("event=request_terminal ")
            })
            .collect();
        assert!(
            lines.iter().all(|line| line.ends_with(" marker_lost=0")),
            "a lost diagnostic marker leaves no order witness:\n{log}"
        );
        let markers: Vec<String> = lines
            .iter()
            .map(|line| {
                let event = line.split(" event=").nth(1).unwrap();
                let (event, detail) = event.split_once(" detail=").unwrap();
                let id = detail.split_whitespace().next().unwrap();
                format!("{event} {id}")
            })
            .collect();
        if markers.len() >= expected.len() || Instant::now() >= deadline {
            break markers;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(markers, expected, "daemon claim/terminal order");
}

#[cfg(unix)]
#[tokio::test]
async fn serve_arms_sigterm_before_banner_to_control_wait() {
    use std::os::unix::net::UnixStream;

    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "signal-window");
    let token = temp.path().join("token");
    // Port 0 avoids a race between probing a free port and serve binding it.
    // The banner reports the actual loopback address.
    let address: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let socket = fixture_socket_paths(&home).socket;
    let mut daemon_command = cli(&home);
    daemon_command.arg("daemon");
    let mut daemon = detached(daemon_command);
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    while UnixStream::connect(&socket).is_err() {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        assert!(
            Instant::now() < deadline,
            "daemon socket not ready: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let original_pid = daemon.0.id();
    let log = temp.path().join("serve.log");
    let mut command = serve(&home, &root, address, &token);
    command
        .env("TRELLIS_TEST_SERVE_AFTER_BANNER_PAUSE", "1")
        .stdout(Stdio::null())
        .stderr(Stdio::from(fs::File::create(&log).unwrap()));
    let mut served = Owned(command.spawn().unwrap());
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if fs::read_to_string(&log)
            .unwrap()
            .contains("Trellis: http://127.0.0.1:")
        {
            break;
        }
        assert!(
            served.0.try_wait().unwrap().is_none(),
            "serve exited before banner: {}",
            fs::read_to_string(&log).unwrap()
        );
        assert!(Instant::now() < deadline, "serve banner not printed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // The opt-in pause keeps the serve select unpolled. Without pre-arming
    // SIGTERM, this signal terminates the process instead of closing control.
    assert_eq!(
        unsafe { libc::kill(served.0.id() as i32, libc::SIGTERM) },
        0
    );
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let status = loop {
        if let Some(status) = served.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "serve did not exit on SIGTERM");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(
        status.success(),
        "serve did not shut down gracefully: {status}; log: {}",
        fs::read_to_string(&log).unwrap()
    );
    assert_eq!(daemon.0.id(), original_pid);
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "daemon owner exited"
    );
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "daemon socket changed"
    );
}

/// Keep a CLI reply bounded even if the daemon accepts a socket but never
/// answers it. Redirect pipes to short-lived files so a verbose child cannot
/// fill a pipe and block before the deadline. `Owned` kills and reaps on timeout.
async fn bounded_cli_capture(
    mut command: Command,
    fixture: &Path,
    timeout: Duration,
) -> Option<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    use std::io::Read;
    let stdout = fixture.join("bounded-cli.out");
    let stderr = fixture.join("bounded-cli.err");
    command
        .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr).unwrap()));
    let mut child = Owned(command.spawn().unwrap());
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            let read_bounded = |path: &Path| {
                let mut captured = Vec::new();
                fs::File::open(path)
                    .unwrap()
                    .take(1_048_576)
                    .read_to_end(&mut captured)
                    .unwrap();
                captured
            };
            return Some((status, read_bounded(&stdout), read_bounded(&stderr)));
        }
        if Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn bounded_cli_capture_reaps_stalled_child() {
    let temp = short_temp();
    let mut stalled = Command::new("cat");
    stalled.stdin(Stdio::piped());
    let started = Instant::now();
    assert!(
        bounded_cli_capture(stalled, temp.path(), Duration::from_millis(100))
            .await
            .is_none()
    );
    // The capture bound fired (None above); this only detects a hang.
    assert!(started.elapsed() < trellis::testing::HANG_BACKSTOP);
}

/// One real daemon/serve cold startup reconciliation per private HOME; no prior daemon session can
/// supply a warm head or make an omitted CLI option look explicit.
#[tokio::test]
async fn cold_serve_startup_preserves_implicit_scip_and_honors_explicit_default_cap() {
    use protobuf::Message;
    use sha2::{Digest, Sha256};
    use std::os::unix::net::UnixStream;

    for explicit_default in [false, true] {
        let temp = short_temp();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let root = checkout(temp.path(), "serveColdOptions");
        let source_path = root.join("sample.js");
        // SCIP [0,9,10] names exactly the one-byte declaration `f`.
        fs::write(&source_path, "function f() {}\nf();\n").unwrap();
        let source_hash = hex::encode(Sha256::digest(fs::read(&source_path).unwrap()));
        let scip_path = temp.path().join("prior.scip");
        let manifest_path = temp.path().join("prior-manifest.json");
        let mut scip = scip::types::Index::new();
        let mut document = scip::types::Document::new();
        document.relative_path = "sample.js".into();
        let mut occurrence = scip::types::Occurrence::new();
        occurrence.range = vec![0, 9, 10];
        occurrence.symbol_roles = 1;
        let scip_label = "scip npm fixture 1 sample.js/f().";
        occurrence.symbol = scip_label.into();
        document.occurrences.push(occurrence);
        scip.documents.push(document);
        fs::write(&scip_path, scip.write_to_bytes().unwrap()).unwrap();
        fs::write(
            &manifest_path,
            serde_json::to_vec(&serde_json::json!({"sample.js":source_hash})).unwrap(),
        )
        .unwrap();
        let indexed = cli(&home)
            .arg("index")
            .arg("--workspace")
            .arg(&root)
            .arg("--max-file-bytes")
            .arg("8192")
            .arg("--scip")
            .arg(&scip_path)
            .arg("--manifest")
            .arg(&manifest_path)
            .output()
            .unwrap();
        assert!(
            indexed.status.success(),
            "prior index failed: {}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        let indexes = fixture_roots(&home).cache.join("indexes");
        let index_db = fs::read_dir(indexes)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap()
            .join("index.db");
        let recorded =
            || -> (Value, i64) {
                let db = rusqlite::Connection::open(&index_db).unwrap();
                let (options, revision): (String, i64) = db.query_row(
                "SELECT reconcile_options,index_revision FROM index_metadata WHERE singleton=1",
                [], |row| Ok((row.get(0)?, row.get(1)?))
            ).unwrap();
                (serde_json::from_str(&options).unwrap(), revision)
            };
        let (before, prior_revision) = recorded();
        assert_eq!(before["maxFileBytes"], 8192);
        assert_eq!(before["scipPath"], scip_path.to_str().unwrap());
        assert_eq!(before["manifestPath"], manifest_path.to_str().unwrap());
        // The prior CLI elected this daemon. End that fixture instance before
        // changing the checkout, so serve tests a cold startup reconciliation instead of racing
        // registration against an active CLI writer.
        assert!(home.join("auto-daemon.pid").exists());
        drop(AutoDaemonGuard::for_home(&home));
        let socket = fixture_socket_paths(&home).socket;
        let stop_deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        while UnixStream::connect(&socket).is_ok() {
            assert!(
                Instant::now() < stop_deadline,
                "prior CLI daemon did not exit"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // A second file forces startup reconciliation while the original SCIP input/hash remains
        // valid and can still decorate the unchanged `f` declaration.
        fs::write(
            root.join("new.js"),
            "function newColdHead() { return 2; }\n",
        )
        .unwrap();

        // Spawn the daemon explicitly, rather than allowing serve to auto-start
        // an untracked process. Both Owned guards reap children on every exit.
        let mut daemon_command = cli(&home);
        daemon_command.arg("daemon");
        let mut daemon = detached(daemon_command);
        let socket = fixture_socket_paths(&home).socket;
        let socket_deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        while UnixStream::connect(&socket).is_err() {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "private daemon exited before serve: {}",
                fixture_daemon_diagnostic(&home, &socket)
            );
            assert!(
                Instant::now() < socket_deadline,
                "private daemon socket did not bind: {}",
                fixture_daemon_diagnostic(&home, &socket)
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let address = free_address();
        let token = temp.path().join("token");
        let mut command = serve(&home, &root, address, &token);
        if explicit_default {
            command.arg("--max-file-bytes").arg("2097152");
        }
        let mut served = detached(command);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        ready(&client, address, &mut served.0, &home).await;
        let h_deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            assert!(
                Instant::now() < h_deadline,
                "cold startup reconciliation did not publish a revision after {prior_revision}"
            );
            let mut command = cli(&home);
            command.arg("status").arg("--workspace").arg(&root);
            let remaining = h_deadline.saturating_duration_since(Instant::now());
            let status =
                bounded_cli_capture(command, &home, remaining.min(Duration::from_secs(2))).await;
            let diagnostic = match status {
                Some((exit, stdout, _stderr)) if exit.success() => {
                    let reply: Value = serde_json::from_slice(&stdout).unwrap();
                    if reply["revision"]["indexRevision"]
                        .as_i64()
                        .is_some_and(|rev| rev > prior_revision)
                    {
                        break;
                    }
                    format!("status has not passed revision {prior_revision}: {reply}")
                }
                Some((_, _, stderr)) => String::from_utf8_lossy(&stderr).into_owned(),
                None => "status subprocess exceeded its two-second deadline".to_owned(),
            };
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "daemon exited during H"
            );
            assert!(
                served.0.try_wait().unwrap().is_none(),
                "serve exited during H"
            );
            assert!(
                Instant::now() < h_deadline,
                "cold startup reconciliation did not publish revision after {prior_revision}: {diagnostic}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let (after, revision) = recorded();
        assert!(
            revision > prior_revision,
            "banner/status alone cannot satisfy startup reconciliation proof"
        );
        if explicit_default {
            assert_eq!(after["maxFileBytes"], 2_097_152);
            assert!(
                after["scipPath"].is_null(),
                "explicit default must clear prior SCIP: {after}"
            );
            assert!(
                after["manifestPath"].is_null(),
                "explicit default must clear prior manifest: {after}"
            );
        } else {
            assert_eq!(after["maxFileBytes"], 8192);
            assert_eq!(after["scipPath"], scip_path.to_str().unwrap());
            assert_eq!(after["manifestPath"], manifest_path.to_str().unwrap());
        }
        // The unchanged `f` source and matching manifest must let startup reconciliation apply
        // the recorded SCIP presentation, not merely keep a path string in storage.
        let mut symbols = cli(&home);
        symbols
            .arg("symbols")
            .arg("--workspace")
            .arg(&root)
            .arg("--search")
            .arg("f");
        let (exit, stdout, stderr) = bounded_cli_capture(symbols, &home, Duration::from_secs(5))
            .await
            .expect("symbols subprocess timed out after H");
        assert!(
            exit.success(),
            "symbols failed: {}",
            String::from_utf8_lossy(&stderr)
        );
        let reply: Value = serde_json::from_slice(&stdout).unwrap();
        let symbol = reply["data"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == "f")
            .expect("unchanged declaration was not indexed");
        if explicit_default {
            assert!(
                symbol["displayLabel"].is_null(),
                "explicit default unexpectedly reused prior SCIP: {symbol}"
            );
        } else {
            assert_eq!(
                symbol["displayLabel"], scip_label,
                "implicit serve did not apply recorded SCIP to startup reconciliation publication"
            );
        }
    }
}

#[test]
fn cold_cli_reads_start_daemon_and_keep_data_envelopes() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "cold-cli");
    let indexed = run(&home, "index", &root);
    assert!(indexed["status"]["catchingUp"].is_boolean());
    let status = run(&home, "status", &root);
    assert!(status["catchingUp"].is_boolean());
    let symbols = run(&home, "symbols", &root);
    assert!(symbols["catchingUp"].is_boolean());
    assert!(
        !symbols["data"]["items"].as_array().unwrap().is_empty(),
        "CLI symbols lost selected items"
    );
    let seed = symbols["data"]["items"][0]["id"].as_str().unwrap();
    let query = cli(&home)
        .arg("query")
        .arg("--workspace")
        .arg(&root)
        .arg("--seed")
        .arg(seed)
        .output()
        .unwrap();
    assert!(
        query.status.success(),
        "{}",
        String::from_utf8_lossy(&query.stderr)
    );
    let query: Value = serde_json::from_slice(&query.stdout).unwrap();
    assert!(query["catchingUp"].is_boolean());
    assert!(query["data"]["nodes"].is_array());
    let gc = cli(&home).arg("gc").arg("--report").output().unwrap();
    assert!(
        gc.status.success(),
        "{}",
        String::from_utf8_lossy(&gc.stderr)
    );
    assert!(serde_json::from_slice::<Value>(&gc.stdout).is_ok());
}

/// A test-elected daemon outlives its electing CLI but not its owning test
/// process (#130), so an unguarded or killed test cannot leak it. The owner
/// has exited but is not yet reaped when the daemon must notice it is gone.
#[test]
fn elected_daemon_exits_after_its_test_owner_process_ends() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "owner");
    let _daemon = AutoDaemonGuard::for_home(&home);
    // A stand-in owner whose lifetime this test controls.
    let mut owner = Owned(Command::new("sleep").arg("30").spawn().unwrap());
    let output = cli(&home)
        .env(trellis::testing::OWNER_PID_ENV, owner.0.id().to_string())
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let daemon: i32 = fs::read_to_string(home.join("auto-daemon.pid"))
        .unwrap()
        .parse()
        .unwrap();
    // The electing CLI has exited; with its owner alive the daemon stays up
    // across several watchdog checks (its idle delay is 30 minutes).
    std::thread::sleep(Duration::from_millis(2500));
    assert!(
        trellis::testing::process_running(daemon),
        "daemon exited while its owner was alive"
    );
    // Killed but deliberately not reaped until the daemon has exited.
    owner.0.kill().unwrap();
    trellis::testing::wait_until("daemon exit after its owner ended", || {
        !trellis::testing::process_running(daemon)
    });
}

#[test]
fn metrics_cli_reports_pinned_envelope_and_typed_selectors() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricscli");
    run(&home, "index", &root);
    let response = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        response.status.success(),
        "{}",
        String::from_utf8_lossy(&response.stderr)
    );
    let envelope: Value = serde_json::from_slice(&response.stdout).unwrap();
    assert!(envelope["catchingUp"].is_boolean());
    assert_eq!(envelope["payload"]["metricsVersion"], "metrics:v1");
    assert_eq!(envelope["payload"]["scope"], "revision");
    assert_eq!(envelope["payload"]["role"], "all");
    assert_eq!(envelope["payload"]["declarations"], serde_json::json!([]));
    assert!(!envelope["payload"]["files"].as_array().unwrap().is_empty());
    let generation = envelope["payload"]["indexGeneration"].as_str().unwrap();
    let revision = envelope["payload"]["indexRevision"].as_u64().unwrap();
    let pin = format!("{generation}:{revision}");
    let file = &envelope["payload"]["files"][0];
    let selected = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .args([
            "--scope",
            "file",
            "--pin",
            &pin,
            "--source-set-id",
            file["sourceSetId"].as_str().unwrap(),
            "--language",
            file["language"].as_str().unwrap(),
            "--path",
            file["path"].as_str().unwrap(),
            "--aggregate",
            "mean",
            "--metrics",
            "loc,fanIn",
        ])
        .output()
        .unwrap();
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let selected: Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(
        selected["payload"]["selectedMetrics"],
        serde_json::json!(["loc", "fanIn"])
    );
    assert_eq!(selected["payload"]["files"].as_array().unwrap().len(), 1);
    assert_eq!(selected["payload"]["directories"], serde_json::json!([]));
    let text = cli(&home)
        .args(["metrics", "--workspace"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("catchingUp="));
    let bad = cli(&home)
        .args(["metrics", "--workspace"])
        .arg(&root)
        .args(["--scope", "file", "--path", "nonexistent.js"])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("invalid_argument"));
    let pin_error = cli(&home)
        .args(["metrics", "--workspace"])
        .arg(&root)
        .args(["--pin", "not-a-pin:1"])
        .output()
        .unwrap();
    assert!(!pin_error.status.success());
    assert!(String::from_utf8_lossy(&pin_error.stderr).contains("invalid_argument"));
    for role in ["", "unknown", "Production"] {
        let bad = cli(&home)
            .args(["metrics", "--json", "--workspace"])
            .arg(&root)
            .args(["--role", role])
            .output()
            .unwrap();
        assert!(!bad.status.success(), "{role}");
        assert!(bad.stdout.is_empty());
        assert!(String::from_utf8_lossy(&bad.stderr).contains("invalid_argument"));
    }
    let duplicate = cli(&home)
        .args(["metrics", "--workspace"])
        .arg(&root)
        .args(["--role", "all", "--role", "test"])
        .output()
        .unwrap();
    assert!(!duplicate.status.success());
    assert!(duplicate.stdout.is_empty());
}

#[test]
fn metrics_protocol_deadline_covers_whole_frame_not_each_read() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    let (mut writer, mut reader) = UnixStream::pair().unwrap();
    let producer = std::thread::spawn(move || {
        for byte in [0u8, 0, 0, 20] {
            if writer.write_all(&[byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(15));
        }
    });
    let deadline = Instant::now() + Duration::from_millis(35);
    let error =
        trellis::daemon::protocol::read_frame_until::<Value>(&mut reader, deadline).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    ));
    producer.join().unwrap();
}

#[test]
fn metrics_client_rejects_wrong_admission_and_partial_result() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::{client, protocol};
    let request = protocol::Request {
        id: 17,
        operation: "metrics".into(),
        payload: serde_json::json!({}),
    };
    for case in ["wrong_id", "not_admitted", "partial_result"] {
        let (left, mut right) = UnixStream::pair().unwrap();
        let peer = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let mut preface = [0];
            right.read_exact(&mut preface).unwrap();
            assert_eq!(preface, [0xff]);
            let _: protocol::Request = protocol::read_frame(&mut right).unwrap();
            protocol::write_frame(
                &mut right,
                &protocol::Reply {
                    id: if case == "wrong_id" { 18 } else { 17 },
                    payload: serde_json::json!({"admitted": case != "not_admitted"}),
                },
            )
            .unwrap();
            if case == "partial_result" {
                right.write_all(&[0, 0, 0, 42, b'{']).unwrap();
            }
        });
        let mut socket = Some(left);
        let start = Instant::now();
        let result = client::call_metrics(
            &request,
            start + Duration::from_millis(250),
            start + Duration::from_secs(1),
            |_| {
                socket.take().ok_or(std::io::Error::new(
                    std::io::ErrorKind::ConnectionRefused,
                    "one attempt",
                ))
            },
        );
        assert_eq!(result.unwrap_err(), client::CallError::DaemonUnavailable);
        peer.join().unwrap();
    }
}

#[test]
fn metrics_preface_rejects_other_operation_and_preserves_ordinary_frames() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::protocol::{self, Reply, Request};
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricsframing");
    run(&home, "index", &root);
    let socket = fixture_socket_paths(&home).socket;
    let request = Request {
        id: 9,
        operation: "status".to_owned(),
        payload: serde_json::json!({"workspace": root, "args": {}}),
    };
    let mut ordinary = UnixStream::connect(&socket).unwrap();
    ordinary
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    protocol::write_frame(&mut ordinary, &request).unwrap();
    let answer: Reply = protocol::read_frame(&mut ordinary).unwrap();
    assert_eq!(answer.id, 9);
    assert!(
        answer.payload["result"]["catchingUp"].is_boolean(),
        "{answer:?}"
    );
    let mut prefaced = UnixStream::connect(&socket).unwrap();
    prefaced
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    protocol::write_preface_until(&mut prefaced, Instant::now() + Duration::from_secs(1)).unwrap();
    protocol::write_frame(&mut prefaced, &request).unwrap();
    assert!(protocol::read_frame::<Reply>(&mut prefaced).is_err());
    let mut again = UnixStream::connect(&socket).unwrap();
    protocol::write_frame(&mut again, &request).unwrap();
    assert!(protocol::read_frame::<Reply>(&mut again).unwrap().payload["result"].is_object());
}

#[test]
fn metrics_client_reassembles_complete_chunks_under_original_deadline() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::{client, protocol};
    let (left, mut right) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let mut preface = [0];
        right.read_exact(&mut preface).unwrap();
        assert_eq!(preface, [0xff]);
        let request: protocol::Request = protocol::read_frame(&mut right).unwrap();
        protocol::write_frame(
            &mut right,
            &protocol::Reply {
                id: request.id,
                payload: serde_json::json!({"admitted": true}),
            },
        )
        .unwrap();
        let body = serde_json::to_vec(
            &serde_json::json!({"result": {"catchingUp": false, "payload": {"scope": "revision"}}}),
        )
        .unwrap();
        for (part, more) in [(body[..10].to_vec(), true), (body[10..].to_vec(), false)] {
            protocol::write_frame(
                &mut right,
                &protocol::Reply {
                    id: request.id,
                    payload: serde_json::json!({"chunk": hex::encode(part), "more": more}),
                },
            )
            .unwrap();
        }
    });
    let request = protocol::Request {
        id: 41,
        operation: "metrics".into(),
        payload: serde_json::json!({}),
    };
    let mut socket = Some(left);
    let now = Instant::now();
    let reply = client::call_metrics(
        &request,
        now + Duration::from_secs(1),
        now + Duration::from_secs(2),
        |_| {
            socket.take().ok_or(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "retry",
            ))
        },
    )
    .unwrap();
    assert_eq!(reply.payload["result"]["payload"]["scope"], "revision");
    server.join().unwrap();
}

#[test]
fn metrics_cli_work_timeout_is_typed_and_does_not_poison_daemon() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricstimeout");
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("TRELLIS_TEST_METRICS_WORK_DEADLINE_MS", "25")
        .env("TRELLIS_TEST_METRICS_WORK_DELAY_MS", "40")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let timed_out = cli(&home)
        .arg("metrics")
        .arg("--json")
        .arg("--workspace")
        .arg(&root)
        .output()
        .unwrap();
    assert!(!timed_out.status.success());
    assert!(timed_out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&timed_out.stderr).contains("deadline_exceeded"),
        "{}",
        String::from_utf8_lossy(&timed_out.stderr)
    );
    assert!(run(&home, "status", &root)["catchingUp"].is_boolean());
}

#[test]
fn metrics_admission_near_ingress_limit_still_allows_later_complete_result() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::{client, protocol};
    let (left, mut right) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let mut preface = [0];
        right.read_exact(&mut preface).unwrap();
        let request: protocol::Request = protocol::read_frame(&mut right).unwrap();
        std::thread::sleep(Duration::from_millis(650));
        protocol::write_frame(
            &mut right,
            &protocol::Reply {
                id: request.id,
                payload: serde_json::json!({"admitted": true}),
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(350));
        protocol::write_frame(
            &mut right,
            &protocol::Reply {
                id: request.id,
                payload: serde_json::json!({"result": {"catchingUp": false}}),
            },
        )
        .unwrap();
    });
    let request = protocol::Request {
        id: 34,
        operation: "metrics".into(),
        payload: serde_json::json!({}),
    };
    let now = Instant::now();
    let mut socket = Some(left);
    let reply = client::call_metrics(
        &request,
        now + Duration::from_millis(950),
        now + Duration::from_secs(3),
        |_| {
            socket.take().ok_or(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "not replayed",
            ))
        },
    )
    .unwrap();
    assert_eq!(reply.payload["result"]["catchingUp"], false);
    server.join().unwrap();
}

#[tokio::test]
async fn metrics_cli_json_matches_authenticated_selected_http_envelope() {
    async fn http_metrics(
        client: &reqwest::Client,
        address: SocketAddr,
        key: &str,
        token: &str,
        query: &str,
    ) -> Value {
        let response = client
            .get(format!(
                "http://{address}/api/checkouts/{key}/metrics{query}"
            ))
            .bearer_auth(token)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK, "{query}");
        response.json().await.unwrap()
    }
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "metricshttp");
    fs::write(
        root.join("roles.rs"),
        r#"fn production() -> usize { 1 }
#[cfg(feature = "test-support")] fn support() -> usize { 2 }
#[cfg(test)] mod tests { fn check(x: bool) -> bool { if x { true } else { false } } }
"#,
    )
    .unwrap();
    fs::create_dir(root.join("tests")).unwrap();
    fs::write(
        root.join("tests/only.js"),
        "function only() { return 1; }\n",
    )
    .unwrap();
    fs::create_dir(root.join("src")).unwrap();
    let source = root.join("src/math.js");
    fs::write(
        &source,
        "function one(a) {\n  if (a) return a;\n  return 0;\n}\nfunction two(b) {\n  if (b) return b;\n  if (b > 1) return 2;\n  return 0;\n}\n",
    )
    .unwrap();
    let address = free_address();
    let token = temp.path().join("token");
    let mut served = detached(serve(&home, &root, address, &token));
    let client = reqwest::Client::new();
    ready(&client, address, &mut served.0, &home).await;
    run(&home, "index", &root);
    let key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .root_key;
    let auth = fs::read_to_string(&token).unwrap();
    let token = auth.trim();
    let http = http_metrics(&client, address, &key, token, "").await;
    let generation = http["payload"]["indexGeneration"].as_str().unwrap();
    let revision = http["payload"]["indexRevision"].as_u64().unwrap();
    let pin = format!("{generation}:{revision}");
    let initial = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .args(["--pin", &pin])
        .output()
        .unwrap();
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let initial: Value = serde_json::from_slice(&initial.stdout).unwrap();
    assert_eq!(initial["payload"], http["payload"]);
    assert_eq!(initial["payload"]["role"], "all");
    assert!(initial["catchingUp"].is_boolean());
    for (role, count, cyclomatic) in [("production", 2, 2), ("all", 3, 4), ("test", 1, 2)] {
        let query = format!(
            "?role={role}&aggregate=sum&indexGeneration={generation}&indexRevision={revision}"
        );
        let http_selected = http_metrics(&client, address, &key, token, &query).await;
        let cli_selected = cli(&home)
            .args(["metrics", "--json", "--workspace"])
            .arg(&root)
            .args(["--pin", &pin, "--role", role, "--aggregate", "sum"])
            .output()
            .unwrap();
        assert!(
            cli_selected.status.success(),
            "{}",
            String::from_utf8_lossy(&cli_selected.stderr)
        );
        let cli_selected: Value = serde_json::from_slice(&cli_selected.stdout).unwrap();
        assert_eq!(cli_selected["payload"], http_selected["payload"]);
        assert_eq!(cli_selected["payload"]["role"], role);
        let mixed = cli_selected["payload"]["files"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["path"] == "roles.rs")
            .unwrap();
        assert_eq!(mixed["declarationCount"], count);
        assert_eq!(mixed["metrics"]["cyclomatic"], cyclomatic);
        assert_eq!(
            mixed["roleCounts"],
            serde_json::json!({"production":2,"test":1})
        );
    }
    let file = http["payload"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "src/math.js")
        .unwrap();
    let source_set = file["sourceSetId"].as_str().unwrap();
    assert_eq!(file["language"], "javascript");

    // The old pin must still select the same source after a newer head is published.
    fs::write(&source, "function newer() { return 99; }\n").unwrap();
    run(&home, "index", &root);
    let file_query = format!(
        "?role=production&scope=file&sourceSetId={source_set}&language=javascript&path=src/math.js&aggregate=mean&metrics=loc,cyclomatic,fanIn"
    );
    let pinned_file_query =
        format!("{file_query}&indexGeneration={generation}&indexRevision={revision}");
    let head = http_metrics(&client, address, &key, token, &file_query).await;
    assert_ne!(head["payload"]["indexRevision"], revision);
    assert_eq!(head["payload"]["files"][0]["declarationCount"], 1);
    let pinned_file = http_metrics(&client, address, &key, token, &pinned_file_query).await;
    let selected = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .args([
            "--role",
            "production",
            "--scope",
            "file",
            "--pin",
            &pin,
            "--source-set-id",
            source_set,
            "--language",
            "javascript",
            "--path",
            "src/math.js",
            "--aggregate",
            "mean",
            "--metrics",
            "loc,cyclomatic,fanIn",
        ])
        .output()
        .unwrap();
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );
    let selected: Value = serde_json::from_slice(&selected.stdout).unwrap();
    assert_eq!(selected["payload"], pinned_file["payload"]);
    assert_eq!(selected["payload"]["indexGeneration"], generation);
    assert_eq!(selected["payload"]["indexRevision"], revision);
    assert_eq!(selected["payload"]["files"][0]["sourceSetId"], source_set);
    assert_eq!(selected["payload"]["files"][0]["language"], "javascript");
    assert_eq!(selected["payload"]["files"][0]["path"], "src/math.js");
    assert_eq!(selected["payload"]["files"][0]["declarationCount"], 2);
    assert_eq!(selected["payload"]["files"][0]["metrics"]["loc"], 4.5);
    assert_eq!(
        selected["payload"]["files"][0]["metrics"]["cyclomatic"],
        2.5
    );
    assert_eq!(
        selected["payload"]["files"][0]["metrics"]["fanIn"]["value"],
        Value::Null
    );
    assert_eq!(
        selected["payload"]["files"][0]["metrics"]["fanIn"]["unavailable"],
        "call resolution not yet produced (#40/#68)"
    );
    assert_eq!(
        selected["payload"]["declarations"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let first = &selected["payload"]["declarations"][0];
    assert_eq!(first["metrics"]["loc"], 4);
    assert_eq!(first["metrics"]["cyclomatic"], 2);
    assert_eq!(selected["payload"]["declarations"][1]["metrics"]["loc"], 5);
    assert_eq!(
        selected["payload"]["declarations"][1]["metrics"]["cyclomatic"],
        3
    );
    let sid = first["sid"].as_str().unwrap();
    assert!(sid.starts_with("sid:v1:"));

    let directory_query = format!(
        "?role=production&scope=directory&sourceSetId={source_set}&path=src&aggregate=mean&metrics=loc,cyclomatic,fanIn&indexGeneration={generation}&indexRevision={revision}"
    );
    let http_directory = http_metrics(&client, address, &key, token, &directory_query).await;
    let directory = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .args([
            "--role",
            "production",
            "--scope",
            "directory",
            "--pin",
            &pin,
            "--source-set-id",
            source_set,
            "--path",
            "src",
            "--aggregate",
            "mean",
            "--metrics",
            "loc,cyclomatic,fanIn",
        ])
        .output()
        .unwrap();
    assert!(
        directory.status.success(),
        "{}",
        String::from_utf8_lossy(&directory.stderr)
    );
    let directory: Value = serde_json::from_slice(&directory.stdout).unwrap();
    assert_eq!(directory["payload"], http_directory["payload"]);
    assert_eq!(directory["payload"]["files"].as_array().unwrap().len(), 1);
    let subtree = directory["payload"]["directories"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["path"] == "src")
        .unwrap();
    assert_eq!(subtree["sourceSetId"], source_set);
    assert_eq!(subtree["declarationCount"], 2);
    assert_eq!(subtree["metrics"]["loc"], 4.5);
    assert_eq!(subtree["metrics"]["cyclomatic"], 2.5);
    assert_eq!(directory["payload"]["declarations"], serde_json::json!([]));

    let declaration_query = format!(
        "?role=production&scope=declaration&sourceSetId={source_set}&path=src/math.js&sid={sid}&aggregate=mean&metrics=loc,cyclomatic,fanIn&indexGeneration={generation}&indexRevision={revision}"
    );
    let http_declaration = http_metrics(&client, address, &key, token, &declaration_query).await;
    let declaration = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .args([
            "--role",
            "production",
            "--scope",
            "declaration",
            "--pin",
            &pin,
            "--source-set-id",
            source_set,
            "--path",
            "src/math.js",
            "--sid",
            sid,
            "--aggregate",
            "mean",
            "--metrics",
            "loc,cyclomatic,fanIn",
        ])
        .output()
        .unwrap();
    assert!(
        declaration.status.success(),
        "{}",
        String::from_utf8_lossy(&declaration.stderr)
    );
    let declaration: Value = serde_json::from_slice(&declaration.stdout).unwrap();
    assert_eq!(declaration["payload"], http_declaration["payload"]);
    assert_eq!(
        declaration["payload"]["declarations"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(declaration["payload"]["declarations"][0], *first);
    assert_eq!(declaration["payload"]["files"], serde_json::json!([]));
    assert_eq!(declaration["payload"]["directories"], serde_json::json!([]));

    let text = cli(&home)
        .args(["metrics", "--workspace"])
        .arg(&root)
        .args([
            "--role",
            "production",
            "--scope",
            "file",
            "--pin",
            &pin,
            "--source-set-id",
            source_set,
            "--language",
            "javascript",
            "--path",
            "src/math.js",
            "--aggregate",
            "mean",
            "--metrics",
            "loc,cyclomatic,fanIn",
        ])
        .output()
        .unwrap();
    assert!(
        text.status.success(),
        "{}",
        String::from_utf8_lossy(&text.stderr)
    );
    let text = String::from_utf8(text.stdout).unwrap();
    assert!(text.contains(&pin));
    assert!(text.contains("catchingUp="));
    assert!(text.contains(source_set) && text.contains("src/math.js"));
    assert!(
        text.contains("\"loc\":4.5") && text.contains("\"cyclomatic\":2.5"),
        "{text}"
    );
    assert!(text.contains("call resolution not yet produced (#40/#68)"));
}
#[test]
fn metrics_client_discards_partial_chunks_after_absolute_result_deadline() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::{client, protocol};
    let (left, mut right) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let mut preface = [0];
        right.read_exact(&mut preface).unwrap();
        let request: protocol::Request = protocol::read_frame(&mut right).unwrap();
        protocol::write_frame(
            &mut right,
            &protocol::Reply {
                id: request.id,
                payload: serde_json::json!({"admitted": true}),
            },
        )
        .unwrap();
        protocol::write_frame(
            &mut right,
            &protocol::Reply {
                id: request.id,
                payload: serde_json::json!({"chunk": hex::encode(b"{\"result\":"), "more": true}),
            },
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(160));
    });
    let request = protocol::Request {
        id: 43,
        operation: "metrics".into(),
        payload: serde_json::json!({}),
    };
    let mut socket = Some(left);
    let now = Instant::now();
    let result = client::call_metrics(
        &request,
        now + Duration::from_millis(60),
        now + Duration::from_millis(100),
        |_| {
            socket.take().ok_or(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "no replay",
            ))
        },
    );
    assert_eq!(result.unwrap_err(), client::CallError::DaemonUnavailable);
    server.join().unwrap();
}

/// A timed-out socket read must not replay a read-only call. On Unix the
/// receive timeout may surface as WouldBlock rather than TimedOut.
#[test]
fn client_read_timeout_does_not_retry_but_connect_failure_does() {
    use std::io;
    use std::os::unix::net::UnixStream;
    use trellis::daemon::{client, protocol};
    let request = protocol::Request {
        id: 44,
        operation: "status".into(),
        payload: serde_json::json!({}),
    };
    let (left, mut right) = UnixStream::pair().unwrap();
    left.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let server = std::thread::spawn(move || {
        let request: protocol::Request = protocol::read_frame(&mut right).unwrap();
        // The peer remains open until the client finishes waiting, ensuring
        // the failure is a read timeout rather than a closed connection.
        let mut byte = [0];
        let eof = std::io::Read::read(&mut right, &mut byte).unwrap();
        (request, eof)
    });
    let mut first = Some(left);
    let mut connects = 0;
    let result = client::call(&request, true, || {
        connects += 1;
        first
            .take()
            .ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionRefused))
    });
    assert_eq!(result.unwrap_err(), client::CallError::DeadlineExceeded);
    assert_eq!(connects, 1, "timed-out read was replayed");
    let (received, eof) = server.join().unwrap();
    assert_eq!(received, request);
    assert_eq!(eof, 0);

    let (left, mut right) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        let received: protocol::Request = protocol::read_frame(&mut right).unwrap();
        protocol::write_frame(
            &mut right,
            &protocol::Reply {
                id: received.id,
                payload: serde_json::json!({"result": true}),
            },
        )
        .unwrap();
        received
    });
    let mut second = Some(left);
    let mut connects = 0;
    let reply = client::call(&request, true, || {
        connects += 1;
        if connects == 1 {
            Err(io::Error::from(io::ErrorKind::ConnectionRefused))
        } else {
            second
                .take()
                .ok_or_else(|| io::Error::from(io::ErrorKind::ConnectionRefused))
        }
    })
    .unwrap();
    assert_eq!(connects, 2, "connection failure should retry");
    assert_eq!(server.join().unwrap(), request);
    assert_eq!(reply.payload, serde_json::json!({"result": true}));
}

/// Plays the daemon for one CLI call: binds the fixture socket, starts
/// the CLI and returns once its whole request has arrived, so a caller's
/// clock starts at the CLI's reply wait, not at its launch.
fn fake_daemon_cli(
    home: &Path,
    root: &Path,
    operation: &str,
    configure: impl FnOnce(&mut Command),
) -> (
    Owned,
    std::os::unix::net::UnixStream,
    trellis::daemon::protocol::Request,
) {
    use std::os::unix::fs::PermissionsExt;
    let socket = fixture_socket_paths(home).socket;
    let dir = socket.parent().unwrap();
    fs::create_dir_all(dir).unwrap();
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let mut command = cli(home);
    command
        .arg(operation)
        .arg("--workspace")
        .arg(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure(&mut command);
    let mut index = Owned(command.spawn().unwrap());
    let mut stream = None;
    trellis::testing::wait_until("the CLI connecting", || {
        assert!(index.0.try_wait().unwrap().is_none(), "index exited early");
        stream = listener.accept().ok().map(|(stream, _)| stream);
        stream.is_some()
    });
    let mut stream = stream.unwrap();
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    stream
        .set_write_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let request: trellis::daemon::protocol::Request =
        trellis::daemon::protocol::read_frame(&mut stream).unwrap();
    assert_eq!(request.operation, operation);
    (index, stream, request)
}

/// Wait for a CLI child to exit; return its status, stdout and stderr.
fn finished(index: &mut Owned) -> (std::process::ExitStatus, String, String) {
    let mut status = None;
    trellis::testing::wait_until("the CLI exiting", || {
        status = index.0.try_wait().unwrap();
        status.is_some()
    });
    let (mut out, mut err) = (String::new(), String::new());
    std::io::Read::read_to_string(&mut index.0.stdout.take().unwrap(), &mut out).unwrap();
    std::io::Read::read_to_string(&mut index.0.stderr.take().unwrap(), &mut err).unwrap();
    (status.unwrap(), out, err)
}

/// The generic CLI reply bound (60 s in production) that once also ended
/// index waits. The #179 index test lowers it so it can outlive it without
/// waiting a minute (#201).
const LOWERED_CLI_REPLY_BOUND: Duration = Duration::from_secs(2);

/// Run `command` with the generic CLI reply bound lowered; return how long to
/// wait so that bound has really passed.
fn lower_cli_reply_bound(command: &mut Command) -> Duration {
    command.env(
        "TRELLIS_TEST_CLI_REPLY_BOUND_MS",
        LOWERED_CLI_REPLY_BOUND.as_millis().to_string(),
    );
    LOWERED_CLI_REPLY_BOUND + Duration::from_millis(500)
}

/// #179: the CLI once gave up on every index reply at the generic CLI reply
/// bound (60 s) after sending its request, with `outcome_unknown`. An index
/// may take longer. The bound is lowered here (#201).
#[test]
fn cli_index_waits_past_the_cli_reply_bound_for_its_reply() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    // Only if the CLI failed to reach this test's socket and elected one.
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "stalled");
    let mut past_bound = Duration::ZERO;
    let (mut index, mut stream, request) = fake_daemon_cli(&home, &root, "index", |command| {
        past_bound = lower_cli_reply_bound(command);
    });
    // The old bound would have expired half a second before this check.
    std::thread::sleep(past_bound);
    assert!(
        index.0.try_wait().unwrap().is_none(),
        "index stopped waiting for its reply"
    );
    let result = serde_json::json!({"publishedRevision": "answered past the bound"});
    trellis::daemon::protocol::write_frame(
        &mut stream,
        &trellis::daemon::protocol::Reply {
            id: request.id,
            payload: serde_json::json!({"result": result}),
        },
    )
    .unwrap();
    let (status, out, err) = finished(&mut index);
    assert!(status.success(), "{err}");
    assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), result);
}

/// A silent daemon after accepting a read-only CLI request has exceeded the
/// socket reply bound, not become unavailable. Receiving the request first
/// synchronizes the test with the beginning of the reply wait.
#[test]
fn cli_read_reply_timeout_is_deadline_exceeded() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "silent-read");
    let (mut status, _stream, _request) = fake_daemon_cli(&home, &root, "status", |command| {
        command.env("TRELLIS_TEST_CLI_REPLY_BOUND_MS", "100");
    });
    let (exit, out, err) = finished(&mut status);
    assert!(!exit.success());
    assert!(out.is_empty(), "read returned partial output: {out}");
    assert!(err.contains("deadline_exceeded"), "{err}");
    assert!(!err.contains("daemon_unavailable"), "{err}");
}

/// #179: the index wait is still bounded. Its backstop (10 minutes, shrunk
/// here) ends a wait with no reply as `outcome_unknown`.
#[test]
fn cli_index_reply_wait_ends_at_its_backstop() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "silent");
    let (mut index, _stream, _request) = fake_daemon_cli(&home, &root, "index", |command| {
        command.env("TRELLIS_TEST_INDEX_REPLY_BOUND_MS", "500");
    });
    let (status, _, err) = finished(&mut index);
    assert!(!status.success());
    assert!(err.contains("outcome_unknown"), "{err}");
}

/// Metrics goes through `call_metrics`, with its own ingress, work and transfer
/// deadlines, not the generic CLI reply bound. This proves one request survives
/// held daemon work without reconnecting or recomputing, and returns a complete
/// multi-frame revision. The metrics deadlines themselves are covered by tests
/// that set `TRELLIS_TEST_METRICS_CLIENT_*`, such as
/// `metrics_real_near_ingress_ack_and_late_multiframe_preserve_original_end`.
#[test]
fn metrics_held_daemon_work_returns_complete_multiframe_revision() {
    struct ReleaseGate(std::path::PathBuf);
    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, b"release");
        }
    }
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricslarge");
    fs::create_dir(root.join("src")).unwrap();
    // About 578 reply bytes a file: enough files for a multi-frame reply
    // (asserted below), and few enough to index quickly (#201).
    const FILES: usize = 2_500;
    for number in 0..FILES {
        fs::write(
            root.join(format!("src/f{number:04}.js")),
            format!("function f{number:04}(x) {{ if (x) return x + 1; return 0; }}\n"),
        )
        .unwrap();
    }
    let gate = temp.path().join("metrics-gate");
    fs::create_dir(&gate).unwrap();
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("TRELLIS_TEST_METRICS_WORK_GATE", &gate)
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let release = ReleaseGate(gate.join("release"));
    let start = Instant::now();
    let mut process = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let admitted = Instant::now() + Duration::from_secs(30);
    loop {
        if fs::metadata(gate.join("started")).is_ok() {
            break;
        }
        assert!(
            Instant::now() < admitted,
            "metrics request did not reach the work gate"
        );
        assert!(
            process.try_wait().unwrap().is_none(),
            "metrics exited before its work gate"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let held = Duration::from_millis(2_500);
    std::thread::sleep(held);
    fs::write(&release.0, b"release").unwrap();
    let result = process.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(start.elapsed() > held);
    let envelope: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(
        envelope["payload"]["files"].as_array().unwrap().len(),
        FILES + 1
    );
    assert_eq!(envelope["payload"]["declarations"], serde_json::json!([]));
    assert!(envelope["catchingUp"].is_boolean());
    let wire = serde_json::to_vec(&serde_json::json!({"result": envelope})).unwrap();
    assert!(
        wire.len() > trellis::daemon::protocol::MAX_FRAME,
        "reply was not multi-frame: {}",
        wire.len()
    );
    assert_eq!(
        fs::read(gate.join("started")).unwrap(),
        b"x",
        "request reconnected and recomputed"
    );
}

#[test]
fn metrics_cli_parser_and_serialization_timeouts_are_atomic() {
    for (stage, delay_key) in [
        ("parse", "TRELLIS_TEST_METRICS_PARSE_DELAY_MS"),
        ("serialize", "TRELLIS_TEST_METRICS_SERIALIZE_DELAY_MS"),
    ] {
        let temp = short_temp();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let _daemon = AutoDaemonGuard::for_home(&home);
        let root = checkout(temp.path(), stage);
        if stage == "parse" {
            let mut source = String::from("function parseSymbol(x) {\n");
            for number in 0..4_000 {
                source.push_str(&format!("if (x) x += {number};\n"));
            }
            source.push_str("return x;\n}\n");
            fs::write(root.join("sample.js"), source).unwrap();
        }
        let hit = temp.path().join(format!("{stage}-hit"));
        let marker_key = if stage == "parse" {
            "TRELLIS_TEST_METRICS_PARSE_HIT_FILE"
        } else {
            "TRELLIS_TEST_METRICS_SERIALIZE_HIT_FILE"
        };
        let indexed = cli(&home)
            .arg("index")
            .arg("--workspace")
            .arg(&root)
            .env("TRELLIS_TEST_METRICS_WORK_DEADLINE_MS", "1500")
            .env(delay_key, "2000")
            .env(marker_key, &hit)
            .env("TRELLIS_TEST_METRICS_SERIALIZE_DELAY_AT_BYTES", "0")
            .output()
            .unwrap();
        assert!(
            indexed.status.success(),
            "{stage}: {}",
            String::from_utf8_lossy(&indexed.stderr)
        );
        let output = cli(&home)
            .arg("metrics")
            .arg("--json")
            .arg("--workspace")
            .arg(&root)
            .output()
            .unwrap();
        assert_eq!(
            fs::read(&hit).unwrap(),
            stage.as_bytes(),
            "{stage} hook did not run"
        );
        assert!(!output.status.success(), "{stage} succeeded after deadline");
        assert!(output.stdout.is_empty(), "{stage} sent partial success");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("deadline_exceeded"),
            "{stage}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(run(&home, "status", &root)["catchingUp"].is_boolean());
    }
}

#[test]
fn metrics_prefaced_trickle_expires_whole_daemon_ingress_without_ack() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricstrickle");
    let gate = temp.path().join("unused-metrics-gate");
    fs::create_dir(&gate).unwrap();
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("TRELLIS_TEST_METRICS_INGRESS_DEADLINE_MS", "800")
        .env("TRELLIS_TEST_METRICS_WORK_GATE", &gate)
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let mut stream = UnixStream::connect(fixture_socket_paths(&home).socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    stream.write_all(&[0xff]).unwrap();
    // Every individual byte arrives before the socket read timeout; the
    // complete length prefix does not arrive before the absolute ingress cap.
    for byte in [0u8, 0, 0, 80] {
        std::thread::sleep(Duration::from_millis(250));
        if stream.write_all(&[byte]).is_err() {
            break;
        }
    }
    let mut one = [0];
    assert!(
        matches!(stream.read(&mut one), Ok(0) | Err(_)),
        "expired request got an ACK"
    );
    assert!(
        !gate.join("started").exists(),
        "metrics work started without admission"
    );
    assert!(run(&home, "status", &root)["catchingUp"].is_boolean());
}

#[test]
fn metrics_peer_closes_at_parser_hook_without_poisoning_daemon() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::protocol::{self, Reply, Request};
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricsclosed");
    let mut source = String::from("function metricsclosedSymbol(x) {\n");
    for number in 0..4_000 {
        source.push_str(&format!("if (x) x += {number};\n"));
    }
    source.push_str("return x;\n}\n");
    fs::write(root.join("sample.js"), source).unwrap();
    let hit = temp.path().join("parser-started");
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("TRELLIS_TEST_METRICS_PARSE_DELAY_MS", "400")
        .env("TRELLIS_TEST_METRICS_PARSE_HIT_FILE", &hit)
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let mut stream = UnixStream::connect(fixture_socket_paths(&home).socket).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    protocol::write_preface_until(&mut stream, deadline).unwrap();
    protocol::write_frame_until(&mut stream, &Request {
        id: 39, operation: "metrics".into(),
        payload: serde_json::json!({"workspace": root, "args": {"scope": "revision", "aggregate": "sum"}}),
    }, deadline).unwrap();
    let admission: Reply = protocol::read_frame_until(&mut stream, deadline).unwrap();
    assert_eq!(admission.payload, serde_json::json!({"admitted": true}));
    let progress_deadline = Instant::now() + Duration::from_secs(5);
    while !hit.exists() {
        assert!(
            Instant::now() < progress_deadline,
            "parser hook did not run"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(stream);
    let later = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        later.status.success(),
        "{}",
        String::from_utf8_lossy(&later.stderr)
    );
    let envelope: Value = serde_json::from_slice(&later.stdout).unwrap();
    assert_eq!(envelope["payload"]["files"].as_array().unwrap().len(), 1);
}

#[test]
fn metrics_cli_low_cap_and_last_row_timeout_never_emit_partial_success() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricslimit");
    let limit_file = temp.path().join("metrics-limit");
    let threshold_file = temp.path().join("metrics-threshold");
    let hit = temp.path().join("metrics-last-row-hit");
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("TRELLIS_TEST_METRICS_LIMIT_FILE", &limit_file)
        .env(
            "TRELLIS_TEST_METRICS_SERIALIZE_DELAY_AT_BYTES_FILE",
            &threshold_file,
        )
        .env("TRELLIS_TEST_METRICS_SERIALIZE_DELAY_MS", "2000")
        .env("TRELLIS_TEST_METRICS_SERIALIZE_HIT_FILE", &hit)
        .env("TRELLIS_TEST_METRICS_WORK_DEADLINE_MS", "500")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let metrics = || {
        cli(&home)
            .args(["metrics", "--json", "--workspace"])
            .arg(&root)
            .output()
            .unwrap()
    };
    let baseline = metrics();
    assert!(
        baseline.status.success(),
        "{}",
        String::from_utf8_lossy(&baseline.stderr)
    );
    let envelope: Value = serde_json::from_slice(&baseline.stdout).unwrap();
    let bytes = serde_json::to_vec(&envelope).unwrap();
    assert!(bytes.len() > 80);
    fs::write(&limit_file, (bytes.len() - 2).to_string()).unwrap();
    let over_cap = metrics();
    assert!(!over_cap.status.success());
    assert!(over_cap.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&over_cap.stderr).contains("response_too_large"),
        "{}",
        String::from_utf8_lossy(&over_cap.stderr)
    );
    fs::remove_file(&limit_file).unwrap();
    fs::write(&threshold_file, (bytes.len() - 80).to_string()).unwrap();
    let last_row = metrics();
    assert_eq!(
        fs::read(&hit).unwrap(),
        b"serialize",
        "late serializer hook did not run"
    );
    assert!(!last_row.status.success());
    assert!(last_row.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&last_row.stderr).contains("deadline_exceeded"),
        "{}",
        String::from_utf8_lossy(&last_row.stderr)
    );
}

#[test]
fn metrics_reconnect_keeps_original_ingress_deadline() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::{client, protocol};
    let (left, mut right) = UnixStream::pair().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::Read;
        let mut preface = [0];
        right.read_exact(&mut preface).unwrap();
        let _: protocol::Request = protocol::read_frame(&mut right).unwrap();
        std::thread::sleep(Duration::from_millis(180));
    });
    let request = protocol::Request {
        id: 78,
        operation: "metrics".into(),
        payload: serde_json::json!({}),
    };
    let start = Instant::now();
    let mut socket = Some(left);
    let mut connects = 0;
    let result = client::call_metrics(
        &request,
        start + Duration::from_millis(110),
        start + Duration::from_secs(1),
        |_| {
            connects += 1;
            socket.take().ok_or(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "unexpected reconnect",
            ))
        },
    );
    assert_eq!(result.unwrap_err(), client::CallError::DaemonUnavailable);
    assert_eq!(connects, 1, "reconnect reset the original ingress budget");
    server.join().unwrap();
}

#[test]
fn metrics_connect_attempt_rejects_late_first_connect_and_late_starter() {
    use std::os::unix::net::{UnixListener, UnixStream};
    use trellis::daemon::client::{self, StartOutcome};
    let (stream, _peer) = UnixStream::pair().unwrap();
    let mut stream = Some(stream);
    let first = client::connect_or_start_metrics(
        || {
            std::thread::sleep(Duration::from_millis(90));
            Ok(stream.take().unwrap())
        },
        || panic!("a successful first connect must not start the daemon"),
        Duration::from_millis(40),
    );
    assert_eq!(first.unwrap_err().kind(), std::io::ErrorKind::TimedOut);

    let mut starter_called = false;
    let first_failed = client::connect_or_start_metrics(
        || {
            std::thread::sleep(Duration::from_millis(90));
            Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        },
        || {
            starter_called = true;
            Ok(StartOutcome::Started)
        },
        Duration::from_millis(40),
    );
    assert_eq!(
        first_failed.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert!(
        !starter_called,
        "a late failed first connect must not start the daemon"
    );

    let temp = short_temp();
    let socket = temp.path().join("late.sock");
    let mut listener = None;
    let started = client::connect_or_start_metrics(
        || UnixStream::connect(&socket),
        || {
            std::thread::sleep(Duration::from_millis(90));
            listener = Some(UnixListener::bind(&socket)?);
            Ok(StartOutcome::Started)
        },
        Duration::from_millis(40),
    );
    assert_eq!(started.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
    assert!(listener.is_some());
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "late starter did bind"
    );
}

#[test]
fn metrics_file_and_declaration_work_deadlines_are_typed() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::protocol::{self, Reply, Request};
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricsfiledeadline");
    let source_set = format!(
        "source-set:v1:path-{}",
        trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .root_key
    );
    let budget_file = temp.path().join("file-work-budget");
    let error_delay_file = temp.path().join("error-transition-delay");
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env(
            "TRELLIS_TEST_METRICS_FILE_WORK_DEADLINE_MS_FILE",
            &budget_file,
        )
        .env(
            "TRELLIS_TEST_METRICS_ERROR_DELAY_MS_FILE",
            &error_delay_file,
        )
        .env("TRELLIS_TEST_METRICS_WORK_DELAY_MS", "800")
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let file = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .args([
            "--scope",
            "file",
            "--source-set-id",
            &source_set,
            "--language",
            "javascript",
            "--path",
            "sample.js",
        ])
        .output()
        .unwrap();
    assert!(
        file.status.success(),
        "{}",
        String::from_utf8_lossy(&file.stderr)
    );
    let file: Value = serde_json::from_slice(&file.stdout).unwrap();
    let sid = file["payload"]["declarations"][0]["sid"].as_str().unwrap();
    assert!(sid.starts_with("sid:v1:"));
    fs::write(&budget_file, "500").unwrap();
    // Only file/declaration use the shortened 30-second class. Revision keeps
    // its independent 300-second class under the same daemon test hook.
    let revision = cli(&home)
        .args(["metrics", "--json", "--workspace"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        revision.status.success(),
        "{}",
        String::from_utf8_lossy(&revision.stderr)
    );
    for scope in ["file", "declaration"] {
        let mut command = cli(&home);
        command
            .args(["metrics", "--json", "--workspace"])
            .arg(&root)
            .args([
                "--scope",
                scope,
                "--source-set-id",
                &source_set,
                "--path",
                "sample.js",
            ]);
        if scope == "file" {
            command.args(["--language", "javascript"]);
        } else {
            command.args(["--sid", sid]);
        }
        let output = command.output().unwrap();
        assert!(!output.status.success(), "{scope} exceeded its work budget");
        assert!(output.stdout.is_empty(), "{scope} emitted partial success");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("deadline_exceeded"),
            "{scope}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // Measure the typed transition from a real matching admission frame,
    // excluding CLI process startup and requiring the error inside the grace.
    fs::write(&error_delay_file, "50").unwrap();
    let mut stream = UnixStream::connect(fixture_socket_paths(&home).socket).unwrap();
    let ingress = Instant::now() + Duration::from_secs(5);
    let request = Request {
        id: 79,
        operation: "metrics".into(),
        payload: serde_json::json!({"workspace": root,
            "args": {"scope": "file", "sourceSetId": source_set,
                "language": "javascript", "path": "sample.js", "aggregate": "sum"}}),
    };
    protocol::write_preface_until(&mut stream, ingress).unwrap();
    protocol::write_frame_until(&mut stream, &request, ingress).unwrap();
    let admission: Reply = protocol::read_frame_until(&mut stream, ingress).unwrap();
    assert_eq!(admission.id, request.id);
    assert_eq!(admission.payload, serde_json::json!({"admitted": true}));
    let work_started = Instant::now();
    let error: Reply =
        protocol::read_frame_until(&mut stream, work_started + Duration::from_secs(2)).unwrap();
    assert_eq!(error.id, request.id);
    assert!(
        error.payload["error"]
            .as_str()
            .unwrap()
            .contains("deadline_exceeded")
    );
    assert!(
        work_started.elapsed() < Duration::from_millis(1100),
        "500ms work + 500ms transition was exceeded: {:?}",
        work_started.elapsed()
    );
    // A transition held beyond the fixed 500ms grace must disconnect rather
    // than publish a typed error or a partial success after expiry.
    fs::write(&error_delay_file, "600").unwrap();
    let mut late = UnixStream::connect(fixture_socket_paths(&home).socket).unwrap();
    let ingress = Instant::now() + Duration::from_secs(5);
    protocol::write_preface_until(&mut late, ingress).unwrap();
    protocol::write_frame_until(&mut late, &request, ingress).unwrap();
    let admission: Reply = protocol::read_frame_until(&mut late, ingress).unwrap();
    assert_eq!(admission.payload, serde_json::json!({"admitted": true}));
    let work_started = Instant::now();
    assert!(
        protocol::read_frame_until::<Reply>(&mut late, work_started + Duration::from_secs(2))
            .is_err(),
        "over-budget error transition wrote a late reply"
    );
    assert!(
        work_started.elapsed() < Duration::from_millis(1600),
        "over-budget transition did not disconnect promptly"
    );
    assert!(run(&home, "status", &root)["catchingUp"].is_boolean());
}

#[test]
fn metrics_real_near_ingress_ack_and_late_multiframe_preserve_original_end() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let _daemon = AutoDaemonGuard::for_home(&home);
    let root = checkout(temp.path(), "metricslate");
    let phase = temp.path().join("phase-order");
    let chunks = temp.path().join("sent-chunks");
    let transfer_budget = temp.path().join("transfer-budget");
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("TRELLIS_TEST_METRICS_INGRESS_DEADLINE_MS", "5000")
        .env("TRELLIS_TEST_METRICS_ACK_DELAY_MS", "3700")
        .env("TRELLIS_TEST_METRICS_WORK_DEADLINE_MS", "3500")
        .env("TRELLIS_TEST_METRICS_WORK_DELAY_MS", "2800")
        .env("TRELLIS_TEST_METRICS_TRANSFER_DEADLINE_MS", "3500")
        .env(
            "TRELLIS_TEST_METRICS_TRANSFER_DEADLINE_MS_FILE",
            &transfer_budget,
        )
        .env("TRELLIS_TEST_METRICS_FORCE_CHUNK_BYTES", "300")
        .env("TRELLIS_TEST_METRICS_LAST_CHUNK_DELAY_MS", "2800")
        .env("TRELLIS_TEST_METRICS_PHASE_ORDER_FILE", &phase)
        .env("TRELLIS_TEST_METRICS_CHUNK_COUNT_FILE", &chunks)
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let metrics = || {
        cli(&home)
            .args(["metrics", "--json", "--workspace"])
            .arg(&root)
            .env("TRELLIS_TEST_METRICS_CLIENT_INGRESS_MS", "5000")
            .env("TRELLIS_TEST_METRICS_CLIENT_WORK_MS", "3500")
            .env("TRELLIS_TEST_METRICS_CLIENT_ERROR_MS", "500")
            .env("TRELLIS_TEST_METRICS_CLIENT_TRANSFER_MS", "3500")
            .output()
            .unwrap()
    };
    let began = Instant::now();
    let complete = metrics();
    let received = began.elapsed();
    assert!(
        complete.status.success(),
        "{}",
        String::from_utf8_lossy(&complete.stderr)
    );
    assert!(
        received > Duration::from_millis(7_500),
        "final frame arrived before the no-ingress end budget: {received:?}"
    );
    assert!(
        received < Duration::from_millis(12_500),
        "final frame exceeded the original additive end budget: {received:?}"
    );
    assert_eq!(
        fs::read(&phase).unwrap(),
        b"AW",
        "work ran before admitted ACK"
    );
    assert!(
        fs::read(&chunks).unwrap().len() > 1,
        "success was not multi-frame"
    );
    let envelope: Value = serde_json::from_slice(&complete.stdout).unwrap();
    assert_eq!(envelope["payload"]["files"].as_array().unwrap().len(), 1);
    assert!(envelope["catchingUp"].is_boolean());

    // Shortening only the server's transfer window leaves earlier chunks on
    // the socket, but the delayed final chunk cannot become a partial success.
    fs::write(&transfer_budget, "500").unwrap();
    fs::remove_file(&chunks).unwrap();
    let began = Instant::now();
    let incomplete = metrics();
    assert!(!incomplete.status.success());
    assert!(
        incomplete.stdout.is_empty(),
        "partial chunks became CLI output"
    );
    assert!(
        String::from_utf8_lossy(&incomplete.stderr).contains("daemon_unavailable"),
        "{}",
        String::from_utf8_lossy(&incomplete.stderr)
    );
    assert!(
        !fs::read(&chunks).unwrap().is_empty(),
        "no partial frame was sent"
    );
    assert!(
        began.elapsed() < Duration::from_millis(12_500),
        "transfer stall was unbounded"
    );
}
