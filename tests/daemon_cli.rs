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
    let mut command = Command::new(env!("CARGO_BIN_EXE_trellis"));
    command
        .env("HOME", home)
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdin(Stdio::null());
    command
}

struct Owned(Child);
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn detached(mut command: Command) -> Owned {
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
    Owned(
        command
            .stdout(Stdio::null())
            .stderr(Stdio::from(fs::File::create(log).unwrap()))
            .spawn()
            .unwrap(),
    )
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
async fn ready(client: &reqwest::Client, address: SocketAddr, child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Ok(reply) = client.get(format!("http://{address}/healthz")).send().await
            && reply.status().is_success()
        {
            return;
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "serve process exited before /healthz"
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
            rx.recv_timeout(Duration::from_secs(10))
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

#[test]
fn daemon_keeps_private_socket_after_transient_accept_errors() {
    use std::os::unix::{fs::MetadataExt, net::UnixStream};
    for fault in ["emfile", "econnaborted"] {
        let temp = short_temp();
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        let root = checkout(temp.path(), "work");
        let mut command = cli(&home);
        command
            .arg("daemon")
            .env("TRELLIS_TEST_DAEMON_ACCEPT_ERROR_ONCE", fault);
        let mut owner = detached(command);
        let socket = fixture_socket_paths(&home).socket;
        let deadline = Instant::now() + Duration::from_secs(10);
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
        for _ in 0..2 {
            let reply = run(&home, "status", &root);
            assert!(reply.is_object(), "{reply}");
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "daemon died after {fault}"
            );
            assert_eq!(fs::metadata(&socket).unwrap().ino(), inode);
        }
    }
}

#[tokio::test]
async fn executable_daemon_is_socket_only_until_explicit_serve_and_registers_two_checkouts() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let first = checkout(temp.path(), "first");
    let second = checkout(temp.path(), "second");
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reserved.local_addr().unwrap();
    let token_file = temp.path().join("token");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let mut daemon_command = cli(&home);
    daemon_command.arg("daemon");
    let mut daemon = detached(daemon_command);
    let socket = fixture_socket_paths(&home).socket;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited before socket bind: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        assert!(
            Instant::now() < deadline,
            "daemon socket was not created: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    // Keep the port reserved until explicit serve. A connect check after
    // releasing it would race with other tests or processes that bind it.
    assert!(daemon.0.try_wait().unwrap().is_none());
    assert!(std::os::unix::net::UnixStream::connect(&socket).is_ok());
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
    drop(reserved);

    let mut first_serve = detached(serve(&home, &first, address, &token_file));
    ready(&client, address, &mut first_serve.0).await;
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
        (reqwest::Method::GET, "/api/jev/status"),
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
    let selected = tokio::time::timeout(Duration::from_secs(20), async {
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
    let jobs = client
        .get(format!(
            "http://{address}/api/checkouts/{first_key}/jobs/current"
        ))
        .bearer_auth(token.trim())
        .send()
        .await
        .unwrap();
    assert_eq!(jobs.status(), reqwest::StatusCode::OK);
    assert!(
        jobs.json::<Value>().await.unwrap().is_null(),
        "old POST accepted a job"
    );
    let wrong_origin = authenticated()
        .header("Origin", "http://not-local.example")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong_origin.status(), reqwest::StatusCode::FORBIDDEN);

    let conflicting_bind = free_address();
    let output = serve(&home, &second, conflicting_bind, &token_file)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bind conflicts"));
    let other_token = temp.path().join("other-token");
    let output = serve(&home, &second, address, &other_token)
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

    let mut second_serve = detached(serve(&home, &second, address, &token_file));
    let deadline = Instant::now() + Duration::from_secs(10);
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
    let mut same_serve = detached(serve(&home, &first, address, &token_file));
    let deadline = Instant::now() + Duration::from_secs(5);
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
        !serde_json::from_slice::<Value>(&symbols.stdout).unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(run(&home, "export", &first).is_object());
    // Index activated this checkout. A conflicting registration cannot replace
    // its options until the active checkout is released.
    let mut conflict = serve(&home, &first, address, &token_file)
        .arg("--max-file-bytes")
        .arg("3145728")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
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
fn finite_cli_index_status_symbols_export_fallback_without_daemon() {
    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "fallback");
    let output = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "index fallback failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = run(&home, "status", &root);
    assert!(status["revision"]["indexRevision"].as_u64().unwrap() >= 1);
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
        "symbols fallback failed: {}",
        String::from_utf8_lossy(&symbols.stderr)
    );
    let symbols: Value = serde_json::from_slice(&symbols.stdout).unwrap();
    assert!(!symbols["items"].as_array().unwrap().is_empty());
    let exported = run(&home, "export", &root);
    assert!(exported.is_object());
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
        let deadline = Instant::now() + Duration::from_secs(10);
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
    let deadline = Instant::now() + Duration::from_secs(10);
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

#[tokio::test]
async fn daemon_scheduler_follows_held_checkout_leader_then_claims_same_cli_request() {
    use std::os::{fd::AsRawFd, unix::net::UnixStream};
    use trellis::store::topology::WorkspaceIdentity;

    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "follower");
    // Establish the durable index before holding its leader lock. Explicit
    // isolated roots avoid changing process-wide HOME in this parallel test.
    let initial = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let initial: Value = serde_json::from_slice(&initial.stdout).unwrap();
    let initial_revision = initial["publishedRevision"]["indexRevision"]
        .as_u64()
        .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    let topology = fixture_roots(&home);
    let leader = topology.leader(&identity).unwrap();
    let held_incarnation = leader.incarnation.to_string();
    let leader_file = fs::File::open(topology.leader_lock(&identity)).unwrap();
    let mut daemon_command = cli(&home);
    daemon_command.arg("daemon");
    let mut daemon = detached(daemon_command);
    let socket = fixture_socket_paths(&home).socket;
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon died before socket readiness: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        assert!(
            Instant::now() < deadline,
            "daemon socket not ready: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // This command must reach the daemon socket, not the no-daemon fallback.
    // Its durable queue row is observed while the leader guard remains alive.
    let stdout = fs::File::create(temp.path().join("index.stdout")).unwrap();
    let stderr_path = temp.path().join("index.stderr");
    let stderr = fs::File::create(&stderr_path).unwrap();
    let mut request = Owned(
        cli(&home)
            .arg("index")
            .arg("--workspace")
            .arg(&root)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap(),
    );
    let queue_path = topology.requests_db(&identity);
    let deadline = Instant::now() + Duration::from_secs(15);
    let (id, seq): (String, i64) = loop {
        let db = rusqlite::Connection::open_with_flags(
            &queue_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let rows: Vec<(String, i64, String)> = db
            .prepare("SELECT id,seq,state FROM requests ORDER BY seq")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        if rows.len() == 2 {
            assert_eq!(rows[1].2, "queued", "daemon claimed despite held leader");
            break (rows[1].0.clone(), rows[1].1);
        }
        assert_eq!(
            rows.len(),
            1,
            "request was unexpectedly duplicated: {rows:?}"
        );
        assert!(
            request.0.try_wait().unwrap().is_none(),
            "CLI exited before daemon accepted request: {}",
            fs::read_to_string(&stderr_path).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "daemon did not accept index request"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    // Multiple scheduler ticks must leave the original ACK queued, its claim
    // empty, and the held leader incarnation unchanged. flock tests the inode,
    // not just the contents of a possibly stale lock pathname.
    let hold_until = Instant::now() + Duration::from_millis(750);
    while Instant::now() < hold_until {
        assert!(
            request.0.try_wait().unwrap().is_none(),
            "request finished during leader hold"
        );
        assert_eq!(
            fs::read_to_string(topology.leader_lock(&identity))
                .unwrap()
                .trim(),
            held_incarnation
        );
        assert_ne!(
            unsafe { libc::flock(leader_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
            0,
            "checkout leader lock was released during hold"
        );
        let db = rusqlite::Connection::open_with_flags(
            &queue_path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let rows: Vec<(String, i64, String, Option<String>)> = db
            .prepare("SELECT id,seq,state,claim_incarnation FROM requests ORDER BY seq")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(rows.len(), 2, "daemon or CLI enqueued twice: {rows:?}");
        assert_eq!(
            (
                &rows[1].0,
                rows[1].1,
                rows[1].2.as_str(),
                rows[1].3.as_ref()
            ),
            (&id, seq, "queued", None),
            "daemon independently claimed while follower"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    drop(leader);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = request.0.try_wait().unwrap() {
            assert!(
                status.success(),
                "request failed: {}",
                fs::read_to_string(&stderr_path).unwrap()
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "scheduler did not complete queued request"
        );
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let output: Value =
        serde_json::from_slice(&fs::read(temp.path().join("index.stdout")).unwrap()).unwrap();
    let revision = output["publishedRevision"]["indexRevision"]
        .as_u64()
        .unwrap();
    assert!(
        revision > initial_revision,
        "daemon may publish mandatory H before the queued request"
    );
    let db = rusqlite::Connection::open_with_flags(
        &queue_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    type CompletedRow = (String, i64, String, Option<String>, Option<i64>);
    let rows: Vec<CompletedRow> = db
        .prepare("SELECT id,seq,state,claim_incarnation,result_revision FROM requests ORDER BY seq")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    assert_eq!(
        rows.len(),
        2,
        "same request must be completed without re-enqueue"
    );
    assert_eq!(
        (&rows[1].0, rows[1].1, rows[1].2.as_str(), rows[1].4),
        (&id, seq, "done", Some(revision as i64))
    );
    assert_ne!(
        rows[1].3.as_deref(),
        Some(held_incarnation.as_str()),
        "held follower leader claimed job"
    );
    assert_eq!(
        rows[1].3.as_deref(),
        Some(
            fs::read_to_string(topology.leader_lock(&identity))
                .unwrap()
                .trim()
        )
    );
    assert!(daemon.0.try_wait().unwrap().is_none());
}

#[tokio::test]
async fn daemon_served_export_survives_multi_frame_cli_reply() {
    use std::os::unix::net::UnixStream;

    let temp = short_temp();
    let home = temp.path().join("home");
    fs::create_dir(&home).unwrap();
    let root = checkout(temp.path(), "large-export");
    let mut source = String::new();
    for i in 0..6000 {
        source.push_str(&format!(
            "function symbol_{i:05}_{}() {{ return {i}; }}\n",
            "x".repeat(140)
        ));
    }
    fs::write(root.join("large.js"), source).unwrap();
    let mut command = cli(&home);
    command.arg("daemon");
    let mut daemon = detached(command);
    let socket = fixture_socket_paths(&home).socket;
    let deadline = Instant::now() + Duration::from_secs(10);
    while UnixStream::connect(&socket).is_err() {
        assert!(
            daemon.0.try_wait().unwrap().is_none(),
            "daemon exited before socket readiness: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        assert!(
            Instant::now() < deadline,
            "daemon socket not ready: {}",
            fixture_daemon_diagnostic(&home, &socket)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let indexed = cli(&home)
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let exported = cli(&home)
        .arg("export")
        .arg("--workspace")
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert!(
        exported.stdout.len() > 1_048_576,
        "fixture did not cross one reply frame: {} bytes",
        exported.stdout.len()
    );
    let graph: Value = serde_json::from_slice(&exported.stdout).unwrap();
    assert!(graph.is_object(), "daemon export lost its JSON payload");
    assert!(daemon.0.try_wait().unwrap().is_none());
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
    // Port 0: a probed free port can be taken by a concurrent test before
    // serve binds it (#131). The banner reports the bound address.
    let address: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let socket = fixture_socket_paths(&home).socket;
    let mut daemon_command = cli(&home);
    daemon_command.arg("daemon");
    let mut daemon = detached(daemon_command);
    let deadline = Instant::now() + Duration::from_secs(10);
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
    let deadline = Instant::now() + Duration::from_secs(10);
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
    let deadline = Instant::now() + Duration::from_secs(5);
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
    assert!(started.elapsed() < Duration::from_secs(2));
}

/// One real daemon/serve cold H per private HOME; no prior daemon session can
/// supply a warm head or make an omitted CLI option look explicit.
#[tokio::test]
async fn cold_serve_h_preserves_implicit_scip_and_honors_explicit_default_cap() {
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
        // A second file forces H while the original SCIP input/hash remains
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
        let socket_deadline = Instant::now() + Duration::from_secs(10);
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
        ready(&client, address, &mut served.0).await;
        let h_deadline = Instant::now() + Duration::from_secs(20);
        loop {
            assert!(
                Instant::now() < h_deadline,
                "cold H did not publish a revision after {prior_revision}"
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
                "cold H did not publish revision after {prior_revision}: {diagnostic}"
            );
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        let (after, revision) = recorded();
        assert!(
            revision > prior_revision,
            "banner/status alone cannot satisfy H proof"
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
        // The unchanged `f` source and matching manifest must let H apply the
        // recorded SCIP presentation, not merely keep a path string in storage.
        let mut export = cli(&home);
        export.arg("export").arg("--workspace").arg(&root);
        let (exit, stdout, stderr) = bounded_cli_capture(export, &home, Duration::from_secs(5))
            .await
            .expect("export subprocess timed out after H");
        assert!(
            exit.success(),
            "export failed: {}",
            String::from_utf8_lossy(&stderr)
        );
        let graph: Value = serde_json::from_slice(&stdout).unwrap();
        let symbol = graph["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["name"] == "f")
            .expect("unchanged declaration was not indexed");
        if explicit_default {
            assert!(
                symbol["displayLabel"].is_null(),
                "explicit default unexpectedly reused prior SCIP: {symbol}"
            );
        } else {
            assert_eq!(
                symbol["displayLabel"], scip_label,
                "implicit serve did not apply recorded SCIP to H publication"
            );
        }
    }
}
