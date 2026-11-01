#![cfg(feature = "test-support")]
//! Real-process MCP socket ownership, framing, and interrupted-session checks.
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::{fs::OpenOptionsExt, io::AsRawFd, net::UnixStream, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};
use trellis::daemon::protocol;

/// Every reply, including every tool result, stays within the response cap;
/// only a `tools/list` catalog may use its separate, larger cap (#165).
fn assert_reply_cap(raw: &[u8]) {
    let value: Value = serde_json::from_slice(raw).expect("one JSON reply");
    let cap = if value.pointer("/result/tools").is_some_and(Value::is_array) {
        trellis::mcp::wire::CATALOG_BYTES
    } else {
        trellis::mcp::wire::RESPONSE_BYTES
    };
    assert!(
        raw.len() <= cap,
        "{}-byte reply exceeds its {cap}-byte cap",
        raw.len()
    );
}

/// Split a relayed daemon frame, `<relay id> <MCP line>` (Decision 0009 §7).
fn relayed(line: &str) -> (u64, Value) {
    let (relay, frame) = line.trim_end().split_once(' ').expect("relay prefix");
    (relay.parse().unwrap(), serde_json::from_str(frame).unwrap())
}

struct Peer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Value>,
    reaped: bool,
    auto_daemon_pid: PathBuf,
}
impl Peer {
    fn start(home: &Path, root: &Path) -> Self {
        Self::start_with_env(home, root, &[])
    }
    fn start_with_env(home: &Path, root: &Path, extra: &[(&str, &std::ffi::OsStr)]) -> Self {
        let stderr = home.join(format!("stderr-{}.log", rand::random::<u64>()));
        let mut child = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
            .arg("mcp")
            .arg("--workspace")
            .arg(root)
            .envs(extra.iter().copied())
            .env("HOME", home)
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("TRELLIS_TEST_DAEMON_PID_FILE", home.join("auto-daemon.pid"))
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::sync_channel(8);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            while reader.read_until(b'\n', &mut line).unwrap_or(0) != 0 {
                assert_reply_cap(&line);
                if tx.send(serde_json::from_slice(&line).unwrap()).is_err() {
                    return;
                }
                line.clear();
            }
        });
        Self {
            stdin: child.stdin.take(),
            child,
            lines,
            reaped: false,
            auto_daemon_pid: home.join("auto-daemon.pid"),
        }
    }
    fn ask(&mut self, id: u64, method: &str, params: Value) -> Value {
        writeln!(
            self.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )
        .unwrap();
        self.lines
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap()
    }
    // Preserve the leader PID until its client group has stopped.
    // The auto-started daemon is in another session and is cleaned up in Drop.
    fn finish_unreaped(&mut self) {
        self.stdin.take();
        let pid = self.child.id() as libc::pid_t;
        let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    info.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            assert_eq!(rc, 0, "waitid: {}", std::io::Error::last_os_error());
            if unsafe { info.assume_init().si_pid() } == pid {
                break;
            }
            assert!(Instant::now() < deadline, "MCP client did not finish");
            std::thread::yield_now();
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        stop_fixture_daemon(self.auto_daemon_pid.parent().unwrap());
    }
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

fn owned_daemon_connection(home: &Path, pid: u32) -> Option<UnixStream> {
    let stream = UnixStream::connect(socket_under(home)?).ok()?;
    if connected_peer_pid(&stream) != Some(pid) {
        return None;
    }
    let output = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    (output.status.success()
        && String::from_utf8_lossy(&output.stdout).trim()
            == format!("{} daemon", env!("CARGO_BIN_EXE_trellis")))
    .then_some(stream)
}

fn stop_fixture_daemon(home: &Path) {
    let marker = home.join("auto-daemon.pid");
    let Ok(raw) = fs::read_to_string(&marker) else {
        return;
    };
    let Ok(pid) = raw.parse::<u32>() else {
        return;
    };
    let Some(connection) = owned_daemon_connection(home, pid) else {
        return;
    };
    let sent = Command::new("/bin/kill")
        .args(["-TERM", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !sent {
        return;
    }
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    while Instant::now() < deadline {
        if owned_daemon_connection(home, pid).is_none() {
            let _ = fs::remove_file(&marker);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // No numeric-PID SIGKILL if the elected process did not stop normally.
    drop(connection);
}

fn socket_under(home: &Path) -> Option<PathBuf> {
    let mut dirs = vec![home.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).ok()?;
            if meta.is_file() && path.file_name().is_some_and(|name| name == "daemon.lock") {
                return Some(trellis::daemon::SocketPaths::new(path.parent()?.parent()?).socket);
            }
            if meta.is_dir() {
                dirs.push(path);
            }
        }
    }
    None
}
fn socket_ready(home: &Path) -> PathBuf {
    let until = Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if let Some(path) = socket_under(home)
            && UnixStream::connect(&path).is_ok()
        {
            return path;
        }
        assert!(Instant::now() < until, "daemon socket not ready");
        std::thread::yield_now();
    }
}
fn assert_elected(home: &Path) {
    let mut dirs = vec![home.to_path_buf()];
    let mut lock = None;
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "daemon.lock") {
                lock = Some(path);
                break;
            }
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(lock.unwrap())
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        -1
    );
}
fn checkout(home: &Path) -> PathBuf {
    let root = home.join("checkout");
    fs::create_dir_all(root.join(".git")).unwrap();
    root
}

#[test]
fn wrong_pid_marker_never_signals_test_owned_unrelated_process() {
    let home = tempfile::tempdir().unwrap();
    let owner = trellis::daemon::SocketOwner::acquire(&trellis::daemon::SocketPaths::new(
        &home.path().join("private-data"),
    ))
    .unwrap()
    .unwrap();
    let mut dummy = Command::new("/bin/sleep").arg("20").spawn().unwrap();
    fs::write(home.path().join("auto-daemon.pid"), dummy.id().to_string()).unwrap();
    stop_fixture_daemon(home.path());
    let alive = dummy.try_wait().unwrap().is_none();
    let _ = dummy.kill();
    let _ = dummy.wait();
    drop(owner);
    assert!(
        alive,
        "stale marker signaled an unrelated test-owned process"
    );
}

#[test]
fn demand_started_daemon_survives_first_client_process_group_death() {
    use std::os::unix::fs::MetadataExt;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let mut first = Peer::start(home.path(), &root);
    let metadata = json!({"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    }});
    let discover = first.ask(1, "server/discover", metadata.clone());
    assert_eq!(discover["id"], 1);
    assert!(discover["result"].is_object(), "{discover}");
    let socket = socket_ready(home.path());
    let inode = fs::metadata(&socket).unwrap().ino();
    let daemon_pid = fs::read_to_string(home.path().join("auto-daemon.pid")).unwrap();
    let group = format!("-{}", first.child.id());
    let killed = Command::new("/bin/kill")
        .args(["-KILL", "--", &group])
        .status()
        .unwrap();
    assert!(killed.success(), "unable to signal isolated client group");
    assert!(!first.child.wait().unwrap().success());
    first.reaped = true;
    // The old group is gone. Do not demand-start another daemon before this check.
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        UnixStream::connect(&socket).is_ok(),
        "daemon died with first client group"
    );
    assert_eq!(fs::metadata(&socket).unwrap().ino(), inode);
    assert_eq!(
        fs::read_to_string(home.path().join("auto-daemon.pid")).unwrap(),
        daemon_pid
    );
    let mut second = Peer::start(home.path(), &root);
    assert_eq!(second.ask(2, "server/discover", metadata)["id"], 2);
    assert_eq!(fs::metadata(&socket).unwrap().ino(), inode);
    drop(second);
    drop(first);
}

#[test]
fn concurrent_clients_share_single_socket_and_survive_bad_private_frame() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let mut first = Peer::start(home.path(), &root);
    let mut second = Peer::start(home.path(), &root);
    let socket = socket_ready(home.path());
    assert_elected(home.path());
    for (id, peer) in [(1, &mut first), (2, &mut second)] {
        let reply = peer.ask(
            id,
            "server/discover",
            json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}),
        );
        assert_eq!(reply["id"], id);
        assert!(reply["result"].is_object(), "{reply}");
    }
    // Cancellation and a following request cross the same live daemon socket.
    // One write lets the daemon's bounded input queue see cancellation before
    // committing an unsent tool result.
    let metadata = json!({"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    }});
    let sequence = [
        json!({"jsonrpc":"2.0","id":"cancelled","method":"tools/call","params":{
            "name":"trellis_find_symbols","arguments":{"schemaVersion":1,"query":"x"},
            "_meta":metadata["_meta"]
        }}),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"cancelled"}}),
        json!({"jsonrpc":"2.0","id":"after-cancel","method":"tools/list","params":metadata}),
    ];
    let burst = sequence.map(|request| request.to_string()).join("\n") + "\n";
    first
        .stdin
        .as_mut()
        .unwrap()
        .write_all(burst.as_bytes())
        .unwrap();
    let mut reply = first
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    if reply["id"] == "cancelled" {
        assert_eq!(reply["result"]["isError"], true);
        reply = first
            .lines
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    }
    assert_eq!(reply["id"], "after-cancel");
    let mut invalid = UnixStream::connect(&socket).unwrap();
    invalid
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    invalid
        .write_all(&((protocol::MAX_FRAME as u32) + 1).to_be_bytes())
        .unwrap();
    let mut one = [0u8; 1];
    use std::io::Read;
    assert_eq!(invalid.read(&mut one).unwrap(), 0);
    assert_eq!(
        first.ask(
            3,
            "tools/list",
            json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}})
        )["id"],
        3
    );
    first.finish_unreaped();
    assert_eq!(
        second.ask(
            4,
            "tools/list",
            json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}})
        )["id"],
        4
    );
    second.finish_unreaped();
    assert_elected(home.path());
    // Both leaders remain unreaped, so their group IDs cannot be recycled.
    for peer in [&mut first, &mut second] {
        unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
        assert!(peer.child.wait().unwrap().success());
        peer.reaped = true;
    }
    assert!(
        !home.path().join("token").exists(),
        "MCP cannot create a browser token"
    );
}

#[test]
fn daemon_refuses_missing_or_different_mcp_identity_before_marker_attachment() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = socket_ready(home.path());
    for identity in [None, Some(json!("old-catalog"))] {
        let mut stream = UnixStream::connect(&socket).unwrap();
        stream
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let mut payload = json!({"workspace":root,"device":0,"inode":0,"relay":true});
        if let Some(identity) = identity {
            payload["mcpIdentity"] = identity;
        }
        protocol::write_frame(
            &mut stream,
            &protocol::Request {
                id: 33,
                operation: "mcp".into(),
                payload,
            },
        )
        .unwrap();
        let reply: protocol::Reply = protocol::read_frame(&mut stream).unwrap();
        assert_eq!(reply.id, 33);
        assert!(
            reply.payload["error"]
                .as_str()
                .unwrap()
                .starts_with(protocol::MCP_RESTART_REQUIRED)
        );
        assert!(!root.join(".git/trellis/workspace-id").exists());
    }
    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn replacement_daemon_with_changed_catalog_requires_mcp_restart() {
    check_daemon_catalog_mismatch(false);
}

#[test]
fn initial_daemon_with_changed_catalog_requires_mcp_restart() {
    check_daemon_catalog_mismatch(true);
}

fn check_daemon_catalog_mismatch(initial: bool) {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = socket_ready(home.path());
    let mut peer = (!initial).then(|| Peer::start(home.path(), &root));
    let metadata = json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}}});
    if let Some(peer) = peer.as_mut() {
        assert!(peer.ask(1, "tools/list", metadata.clone())["result"]["tools"].is_array());
    }
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    let mut dirs = vec![home.path().to_path_buf()];
    let mut data = None;
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "daemon.lock") {
                data = path.parent().and_then(Path::parent).map(Path::to_path_buf);
                break;
            }
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    let owner =
        trellis::daemon::SocketOwner::acquire(&trellis::daemon::SocketPaths::new(&data.unwrap()))
            .unwrap()
            .unwrap();
    let listener = owner.listener().try_clone().unwrap();
    let (release, released) = mpsc::sync_channel(1);
    let fake = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let attach: protocol::Request = protocol::read_frame(&mut stream).unwrap();
        assert_eq!(attach.payload["mcpIdentity"], protocol::mcp_identity());
        protocol::write_frame(
            &mut stream,
            &protocol::Reply {
                id: attach.id,
                payload: json!({"result":true,"mcpIdentity":"changed-catalog"}),
            },
        )
        .unwrap();
        let mut line = String::new();
        assert_eq!(
            BufReader::new(stream).read_line(&mut line).unwrap(),
            0,
            "relay must not forward MCP traffic to an incompatible daemon"
        );
        listener.set_nonblocking(true).unwrap();
        released
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "restart-required relay must not attach repeatedly"
        );
    });
    let mut peer = peer.unwrap_or_else(|| Peer::start(home.path(), &root));
    for id in [2, 3] {
        let mut params = metadata.clone();
        params["name"] = json!("trellis_workspace_describe");
        params["arguments"] = json!({"schemaVersion":1});
        let response = peer.ask(id, "tools/call", params);
        assert_eq!(response["id"], id);
        assert_eq!(
            response["error"]["data"]["code"],
            protocol::MCP_RESTART_REQUIRED
        );
        assert_eq!(response["error"]["data"]["retryable"], false);
        assert_eq!(response["error"]["code"], -32603);
        assert_eq!(response["error"]["message"], protocol::MCP_RESTART_MESSAGE);
        assert!(response.get("result").is_none());
    }
    release.send(()).unwrap();
    fake.join().unwrap();
    assert!(socket.exists());
}

#[test]
fn daemon_death_interrupts_session_and_next_launch_recovers() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = socket_ready(home.path());
    let mut stale_launch = UnixStream::connect(&socket).unwrap();
    protocol::write_frame(
        &mut stale_launch,
        &protocol::Request {
            id: 33,
            operation: "mcp".into(),
            payload: json!({"workspace":root,"device":0,"inode":0,"mcpIdentity":protocol::mcp_identity()}),
        },
    )
    .unwrap();
    let refused: protocol::Reply = protocol::read_frame(&mut stale_launch).unwrap();
    assert_eq!(refused.id, 33);
    assert!(
        refused.payload["error"]
            .as_str()
            .unwrap()
            .contains("root_changed")
    );
    assert!(!root.join(".git/trellis/workspace-id").exists());
    let mut client = Peer::start(home.path(), &root);
    assert_eq!(
        client.ask(
            1,
            "server/discover",
            json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}})
        )["id"],
        1
    );
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(
        client.child.try_wait().unwrap().is_none(),
        "original MCP client exited"
    );
    let recovered = client.ask(
        2,
        "server/discover",
        json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}),
    );
    assert_eq!(recovered["id"], 2);
    assert!(recovered["result"].is_object(), "{recovered}");
    client.finish_unreaped();
    assert_elected(home.path());
    unsafe { libc::kill(-(client.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(client.child.wait().unwrap().success());
    client.reaped = true;
}

// Observe live attachments without selecting a checkout or scanning index directories.
fn active_checkout_count(home: &Path, root: &Path) -> Value {
    let token_file = home.join("restart-token");
    let mut control = UnixStream::connect(socket_ready(home)).unwrap();
    control
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    protocol::write_frame(
        &mut control,
        &protocol::Request {
            id: 50,
            operation: "serve".into(),
            payload: json!({"workspace":root,"bind":"127.0.0.1:0",
            "tokenFile":token_file,"options":{}}),
        },
    )
    .unwrap();
    let listener: protocol::Reply = protocol::read_frame(&mut control).unwrap();
    let address = listener.payload["result"]
        .as_str()
        .expect("browser listener address");
    let token = fs::read_to_string(token_file).unwrap();
    let mut http = std::net::TcpStream::connect(address).unwrap();
    http.set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    write!(http, "GET /api/daemon/status HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {}\r\nConnection: close\r\n\r\n", token.trim()).unwrap();
    let mut body = String::new();
    std::io::Read::read_to_string(&mut http, &mut body).unwrap();
    let body: Value = serde_json::from_str(body.split_once("\r\n\r\n").unwrap().1).unwrap();
    body["activeCheckouts"].clone()
}

#[test]
fn reconnect_handshake_skips_a_replaced_selection_and_keeps_launch_usable() {
    use trellis::store::topology::WorkspaceIdentity;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let launch = WorkspaceIdentity::discover_unattached(Some(&root), &root).unwrap();
    let selected = home.path().join("selected");
    fs::create_dir_all(selected.join(".git")).unwrap();
    let identity = WorkspaceIdentity::discover_unattached(Some(&selected), &selected)
        .unwrap()
        .attach_marker()
        .unwrap();
    let remembered = protocol::McpAttachment::from_identity(&identity);
    fs::rename(&selected, selected.with_extension("moved")).unwrap();
    fs::create_dir_all(selected.join(".git")).unwrap();
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let socket = socket_ready(home.path());
    let mut stream = UnixStream::connect(&socket).unwrap();
    stream
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    protocol::write_frame(
        &mut stream,
        &protocol::Request {
            id: 1,
            operation: "mcp".into(),
            payload: json!({"workspace":launch.root,"device":launch.device,"inode":launch.inode,
            "relay":true,"mcpIdentity":protocol::mcp_identity(),"attachments":[remembered]}),
        },
    )
    .unwrap();
    let reply: protocol::Reply = protocol::read_frame(&mut stream).unwrap();
    assert_eq!(reply.payload["result"], true, "{reply:?}");
    assert!(
        !selected.join(".git/trellis/workspace-id").exists(),
        "reconnect attached the replacement's marker"
    );
    // The same connection stays usable for its launch, not a successor root.
    writeln!(
        stream,
        "2 {}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"trellis_workspace_describe","arguments":{"schemaVersion":1},
            "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}}}})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let (_, answer) = relayed(&line);
    assert_eq!(
        answer["result"]["structuredContent"]["workspace"],
        json!(launch.root)
    );
    assert_eq!(active_checkout_count(home.path(), &root), 1);
    drop(stream);
    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn reconnect_skips_changed_membership_but_restores_another_selection() {
    use trellis::store::topology::WorkspaceIdentity;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let git = |cwd: &Path, args: &[&str]| {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&root, &["init", "--quiet"]);
    git(
        &root,
        &["commit", "--allow-empty", "--quiet", "-m", "fixture"],
    );
    let changed = home.path().join("changed");
    let valid = home.path().join("valid");
    for selected in [&changed, &valid] {
        git(
            &root,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                selected.to_str().unwrap(),
                "HEAD",
            ],
        );
    }
    let launch = WorkspaceIdentity::discover_unattached(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let changed_identity = WorkspaceIdentity::discover_unattached(Some(&changed), &changed)
        .unwrap()
        .attach_marker()
        .unwrap();
    let valid_identity = WorkspaceIdentity::discover_unattached(Some(&valid), &valid)
        .unwrap()
        .attach_marker()
        .unwrap();
    let other = home.path().join("other");
    fs::create_dir(&other).unwrap();
    git(&other, &["init", "--quiet"]);
    // Keep root inode, Git directory and UUID unchanged; only repository
    // membership changes, which the relay's identity filter cannot detect.
    let pointer = fs::read_to_string(changed.join(".git")).unwrap();
    let admin = Path::new(pointer.trim().strip_prefix("gitdir: ").unwrap());
    fs::write(
        admin.join("commondir"),
        format!("{}\n", other.join(".git").display()),
    )
    .unwrap();
    changed_identity.verify_readonly().unwrap();
    assert_eq!(
        changed_identity.git_common_dir().unwrap(),
        other.join(".git").canonicalize().unwrap()
    );
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stream = UnixStream::connect(socket_ready(home.path())).unwrap();
    stream
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    protocol::write_frame(
        &mut stream,
        &protocol::Request {
            id: 1,
            operation: "mcp".into(),
            payload: json!({"workspace":launch.root,"device":launch.device,"inode":launch.inode,
            "relay":true,"mcpIdentity":protocol::mcp_identity(),"attachments":[protocol::McpAttachment::from_identity(&changed_identity),
                protocol::McpAttachment::from_identity(&valid_identity)]}),
        },
    )
    .unwrap();
    let reply: protocol::Reply = protocol::read_frame(&mut stream).unwrap();
    assert_eq!(reply.payload["result"], true, "{reply:?}");
    writeln!(
        stream,
        "2 {}",
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
        "params":{"name":"trellis_workspace_describe","arguments":{"schemaVersion":1},
            "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}}}})
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let (_, answer) = relayed(&line);
    assert_eq!(
        answer["result"]["structuredContent"]["workspace"],
        json!(launch.root)
    );
    assert_eq!(
        active_checkout_count(home.path(), &root),
        2,
        "invalid membership blocked launch or the other valid attachment"
    );
    drop(stream);
    daemon.kill().unwrap();
    daemon.wait().unwrap();
}

#[test]
fn legacy_mcp_reconnect_restores_handshake_without_new_client() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let git = |cwd: &Path, args: &[&str]| {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&root, &["init", "--quiet"]);
    git(
        &root,
        &["commit", "--allow-empty", "--quiet", "-m", "fixture"],
    );
    let linked = home.path().join("linked");
    git(
        &root,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    let linked = linked.canonicalize().unwrap();
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _socket = socket_ready(home.path());
    let mut peer = Peer::start(home.path(), &root);
    let init = peer.ask(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-11-25", "capabilities":{},
            "clientInfo":{"name":"regression","version":"1"}
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0", "method":"notifications/initialized"
        })
    )
    .unwrap();
    assert_eq!(peer.ask(2, "tools/list", json!({}))["id"], 2);
    let selected = peer.ask(
        20,
        "tools/call",
        json!({
            "name":"trellis_workspace_describe",
            "arguments":{"schemaVersion":1,"workspace":linked}
        }),
    );
    assert_eq!(
        selected["result"]["structuredContent"]["workspace"],
        json!(linked)
    );
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    assert!(peer.child.try_wait().unwrap().is_none());
    // Reconnect without selecting again: retained attachments must not change
    // the contract's launch-workspace default.
    let launch = peer.ask(
        30,
        "tools/call",
        json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}),
    );
    assert_eq!(
        launch["result"]["structuredContent"]["workspace"],
        json!(root.canonicalize().unwrap())
    );
    assert_eq!(
        active_checkout_count(home.path(), &root),
        2,
        "linked attachment was not restored before another explicit selection"
    );
    let result = peer.ask(
        3,
        "tools/call",
        json!({
            "name":"trellis_workspace_describe",
            "arguments":{"schemaVersion":1,"workspace":linked}
        }),
    );
    assert_eq!(result["id"], 3, "{result}");
    assert_eq!(
        result["result"]["structuredContent"]["workspace"],
        json!(linked)
    );
    assert_eq!(result["result"]["structuredContent"]["requestId"], 3);
    peer.finish_unreaped();
    assert_elected(home.path());
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(peer.child.wait().unwrap().success());
    peer.reaped = true;
}

/// While a call is held at the daemon's send decision, the relay keeps
/// answering input (Decision 0009 §7): protocol errors locally, at most 16
/// outstanding requests and 4 outstanding calls (the excess refused at once,
/// calls in the pre-resolution shape), an outstanding ID refused as a
/// duplicate, and a cancelled ID reusable. A cancel written in the burst still
/// suppresses the held call.
#[test]
fn cancellation_precedes_invalid_queue_and_sixteen_follow_ons() {
    use std::os::unix::net::UnixListener;
    for variant in [
        "protocol",
        "requests",
        "calls",
        "duplicate",
        "duplicate-cancel",
    ] {
        let home = tempfile::tempdir().unwrap();
        let root = checkout(home.path());
        let phase_path = PathBuf::from(format!(
            "/tmp/trellis-107-mcp-{}.sock",
            rand::random::<u64>()
        ));
        let listener = UnixListener::bind(&phase_path).unwrap();
        let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
            .arg("daemon")
            .env("HOME", home.path())
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("TRELLIS_TEST_MCP_PHASE", "launch_final")
            .env("TRELLIS_TEST_MCP_PHASE_SOCKET", &phase_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _ = socket_ready(home.path());
        let mut peer = Peer::start(home.path(), &root);
        let metadata = json!({"_meta":{
            "io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientCapabilities":{}
        }});
        let call = |id: Value| {
            json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":"trellis_workspace_describe",
                    "arguments":{"schemaVersion":1},"_meta":metadata["_meta"]}})
        };
        let list =
            |id: u64| json!({"jsonrpc":"2.0","id":id,"method":"tools/list","params":metadata});
        let cancel = |id: Value| json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}});
        writeln!(peer.stdin.as_mut().unwrap(), "{}", call(json!("cancel-A"))).unwrap();
        let hold = |listener: &UnixListener| {
            let (phase, _) = listener.accept().unwrap();
            phase
                .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
                .unwrap();
            let mut stage = String::new();
            BufReader::new(phase.try_clone().unwrap())
                .read_line(&mut stage)
                .unwrap();
            assert_eq!(stage, "launch_final\n");
            phase
        };
        let mut held = hold(&listener);
        let mut burst = String::new();
        let mut push = |frame: String| {
            burst.push_str(&frame);
            burst.push('\n');
        };
        match variant {
            "protocol" => {
                push("{bad json}".into());
                push("x".repeat(16_385));
            }
            "requests" => (100..118).for_each(|id| push(list(id).to_string())),
            "calls" => (200..204).for_each(|id| push(call(json!(id)).to_string())),
            "duplicate" => {
                push(list(8).to_string());
                push(list(8).to_string());
            }
            _ => {
                push(list(8).to_string());
                push(list(8).to_string());
                push(cancel(json!(8)).to_string());
                push(list(8).to_string());
            }
        }
        // With calls, A must still be outstanding when the fourth arrives: its
        // cancel follows the refusal instead of riding in the burst.
        if variant != "calls" {
            push(cancel(json!("cancel-A")).to_string());
        }
        peer.stdin
            .as_mut()
            .unwrap()
            .write_all(burst.as_bytes())
            .unwrap();
        let next = || {
            peer.lines
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap()
        };
        // Local answers come while the daemon still holds A.
        match variant {
            "protocol" => {
                let invalid = next();
                assert_eq!(invalid["id"], Value::Null, "{invalid}");
                assert_eq!(invalid["error"]["code"], -32700, "{invalid}");
                let oversized = next();
                assert_eq!(oversized["id"], Value::Null, "{oversized}");
                assert_eq!(oversized["error"]["code"], -32600, "{oversized}");
            }
            "requests" => {
                // Both are refused locally; their order is not a contract.
                let mut refused_ids = Vec::new();
                for _ in 0..2 {
                    let refused = next();
                    assert_eq!(refused["error"]["code"], -32000, "{refused}");
                    assert_eq!(refused["error"]["data"]["code"], "too_many_requests");
                    // #528: the relay names its per-connection limit.
                    assert_eq!(
                        refused["error"]["message"],
                        "Too many outstanding requests on this connection (limit 16)"
                    );
                    refused_ids.push(refused["id"].as_u64().unwrap());
                }
                refused_ids.sort_unstable();
                assert_eq!(refused_ids, vec![116, 117]);
            }
            "calls" => {
                // A and three more are outstanding: the fourth is refused.
                let refused = next();
                assert_eq!(refused["id"], 203, "{refused}");
                let envelope = &refused["result"]["structuredContent"];
                assert_eq!(envelope["error"]["code"], "too_many_requests", "{refused}");
                assert_eq!(envelope["error"]["retryable"], true);
                // #528: the relay, which agents talk to, names the per-connection limit.
                assert_eq!(
                    envelope["error"]["message"],
                    "Too many concurrent tool calls on this connection (limit 4); wait for one to finish"
                );
                assert_eq!(
                    envelope["error"]["attemptedWorkspace"]["value"],
                    json!(root.canonicalize().unwrap())
                );
                assert!(envelope.get("workspace").is_none(), "{refused}");
                assert!(envelope.get("catchingUp").is_none(), "{refused}");
                writeln!(
                    peer.stdin.as_mut().unwrap(),
                    "{}",
                    cancel(json!("cancel-A"))
                )
                .unwrap();
            }
            "duplicate" => {
                let duplicate = next();
                assert_eq!(duplicate["id"], 8, "{duplicate}");
                assert_eq!(duplicate["error"]["code"], -32600, "{duplicate}");
            }
            // The cancel removes the original and its duplicate refusal.
            _ => {}
        }
        held.write_all(b"x").unwrap();
        match variant {
            "requests" => {
                for id in 100..116 {
                    let reply = next();
                    assert_eq!(reply["id"], id, "{reply}");
                    assert!(reply["result"].is_object(), "{reply}");
                }
            }
            "calls" => {
                let mut answered = Vec::new();
                for _ in 0..3 {
                    hold(&listener).write_all(b"x").unwrap();
                    let reply = next();
                    assert!(reply["result"].is_object(), "{reply}");
                    answered.push(reply["id"].as_u64().unwrap());
                }
                answered.sort_unstable();
                assert_eq!(answered, vec![200, 201, 202]);
            }
            "duplicate" => {
                // The original ID8 is answered once the daemon is released.
                let original = next();
                assert_eq!(original["id"], 8, "{original}");
                assert!(original["result"].is_object(), "{original}");
            }
            "duplicate-cancel" => {
                // The cancelled first ID8 and its duplicate are suppressed;
                // its reuse answers.
                let reused = next();
                assert_eq!(reused["id"], 8, "{reused}");
                assert!(reused["result"].is_object(), "{reused}");
            }
            _ => {}
        }
        assert!(
            peer.lines.recv_timeout(Duration::from_millis(200)).is_err(),
            "the cancelled call or a suppressed reply produced stdout"
        );
        peer.finish_unreaped();
        unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
        assert!(peer.child.wait().unwrap().success());
        peer.reaped = true;
        daemon.kill().unwrap();
        daemon.wait().unwrap();
        fs::remove_file(&phase_path).unwrap();
    }
}

#[test]
fn duplicate_within_bounded_pending_queue_is_rejected_before_first_completes() {
    use std::os::unix::net::UnixListener;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let phase_path = PathBuf::from(format!(
        "/tmp/trellis-107-dup-{}.sock",
        rand::random::<u64>()
    ));
    let listener = UnixListener::bind(&phase_path).unwrap();
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TRELLIS_TEST_MCP_PHASE", "launch_final")
        .env("TRELLIS_TEST_MCP_PHASE_SOCKET", &phase_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = socket_ready(home.path());
    let mut peer = Peer::start(home.path(), &root);
    let meta = json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}}});
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0","id":1,
        "method":"tools/call","params":{"name":"trellis_workspace_describe",
            "arguments":{"schemaVersion":1},"_meta":meta["_meta"]}})
    )
    .unwrap();
    let (mut phase, _) = listener.accept().unwrap();
    phase
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let mut stage = String::new();
    BufReader::new(phase.try_clone().unwrap())
        .read_line(&mut stage)
        .unwrap();
    assert_eq!(stage, "launch_final\n");
    let mut burst = String::new();
    for id in [8, 8, 9] {
        burst.push_str(
            &json!({"jsonrpc":"2.0","id":id,
            "method":"tools/list","params":meta})
            .to_string(),
        );
        burst.push('\n');
    }
    peer.stdin
        .as_mut()
        .unwrap()
        .write_all(burst.as_bytes())
        .unwrap();
    let duplicate = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("second queued ID8 must be rejected before A is released");
    assert_eq!(duplicate["id"], 8, "{duplicate}");
    assert_eq!(duplicate["error"]["code"], -32600, "{duplicate}");
    // A is still held at this point: the duplicate must be rejected at admission.
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":1}
        })
    )
    .unwrap();
    // The relay pipelines both on A's connection (Decision 0009 §7): they are
    // answered, in order, once A's held send decision is released.
    phase.write_all(b"x").unwrap();
    let first = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(first["id"], 8, "{first}");
    assert!(first["result"].is_object(), "{first}");
    let next = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(next["id"], 9, "{next}");
    assert!(next["result"].is_object(), "{next}");
    let reused = peer.ask(8, "tools/list", meta);
    assert_eq!(reused["id"], 8, "{reused}");
    assert!(reused["result"].is_object(), "{reused}");
    assert!(peer.lines.recv_timeout(Duration::from_millis(200)).is_err());
    peer.finish_unreaped();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(peer.child.wait().unwrap().success());
    peer.reaped = true;
    fs::remove_file(&phase_path).unwrap();
}

#[test]
fn stdin_eof_cancels_in_flight_call_without_stdout() {
    use std::os::unix::net::UnixListener;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let phase_path = PathBuf::from(format!(
        "/tmp/trellis-107-eof-{}.sock",
        rand::random::<u64>()
    ));
    let listener = UnixListener::bind(&phase_path).unwrap();
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TRELLIS_TEST_MCP_PHASE", "launch_final")
        .env("TRELLIS_TEST_MCP_PHASE_SOCKET", &phase_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = socket_ready(home.path());
    let mut peer = Peer::start(home.path(), &root);
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","id":91,"method":"tools/call",
            "params":{"name":"trellis_workspace_describe","arguments":{"schemaVersion":1},
            "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}}}
        })
    )
    .unwrap();
    let (mut phase, _) = listener.accept().unwrap();
    phase
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let mut stage = String::new();
    BufReader::new(phase.try_clone().unwrap())
        .read_line(&mut stage)
        .unwrap();
    assert_eq!(stage, "launch_final\n");
    peer.stdin.take();
    phase.write_all(b"x").unwrap();
    peer.finish_unreaped();
    assert!(
        peer.lines.recv_timeout(Duration::from_secs(1)).is_err(),
        "pending result after stdin EOF"
    );
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(peer.child.wait().unwrap().success());
    peer.reaped = true;
    fs::remove_file(&phase_path).unwrap();
}

#[test]
fn canceled_read_is_not_replayed_after_daemon_death() {
    use std::os::unix::net::UnixListener;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let phase_path = PathBuf::from(format!(
        "/tmp/trellis-107-kill-{}.sock",
        rand::random::<u64>()
    ));
    let listener = UnixListener::bind(&phase_path).unwrap();
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TRELLIS_TEST_MCP_PHASE", "launch_final")
        .env("TRELLIS_TEST_MCP_PHASE_SOCKET", &phase_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = socket_ready(home.path());
    let mut peer = Peer::start(home.path(), &root);
    let metadata = json!({"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    }});
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","id":"cancel-A","method":"tools/call",
            "params":{"name":"trellis_workspace_describe",
                "arguments":{"schemaVersion":1},"_meta":metadata["_meta"]}
        })
    )
    .unwrap();
    let (phase, _) = listener.accept().unwrap();
    phase
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let mut stage = String::new();
    BufReader::new(phase.try_clone().unwrap())
        .read_line(&mut stage)
        .unwrap();
    assert_eq!(stage, "launch_final\n");
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":"cancel-A"}
        })
    )
    .unwrap();
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    drop(phase);
    assert!(peer.child.try_wait().unwrap().is_none());
    let reply = peer.ask(5, "tools/list", metadata);
    assert_eq!(reply["id"], 5, "{reply}");
    assert!(reply["result"].is_object(), "{reply}");
    assert!(
        peer.lines.recv_timeout(Duration::from_millis(200)).is_err(),
        "canceled read was replayed"
    );
    peer.finish_unreaped();
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(peer.child.wait().unwrap().success());
    peer.reaped = true;
    fs::remove_file(&phase_path).unwrap();
}

#[test]
fn legacy_completed_initialize_cancel_does_not_abort_synthetic_reattach() {
    check_legacy_synthetic_reattach_cancel(1, false);
}

#[test]
fn legacy_pending_call_cancel_during_synthetic_reattach_is_silent() {
    check_legacy_synthetic_reattach_cancel(5, false);
}

#[test]
fn legacy_cancel_during_failed_synthetic_reattach_is_silent() {
    check_legacy_synthetic_reattach_cancel(5, true);
}

fn check_legacy_synthetic_reattach_cancel(cancel_id: u64, fail_setup: bool) {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = socket_ready(home.path());
    let mut peer = Peer::start(home.path(), &root);
    let initialized = peer.ask(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"regression","version":"1"}
        }),
    );
    assert_eq!(initialized["id"], 1);
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","method":"notifications/initialized"
        })
    )
    .unwrap();
    assert_eq!(peer.ask(4, "tools/list", json!({}))["id"], 4);
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    let mut dirs = vec![home.path().to_path_buf()];
    let mut data = None;
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "daemon.lock") {
                data = path.parent().and_then(Path::parent).map(Path::to_path_buf);
                break;
            }
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    let owner =
        trellis::daemon::SocketOwner::acquire(&trellis::daemon::SocketPaths::new(&data.unwrap()))
            .unwrap()
            .expect("killed daemon released singleton lock");
    let listener = owner.listener().try_clone().unwrap();
    let (connected_tx, connected_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let fake = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let attach: protocol::Request = protocol::read_frame(&mut socket).unwrap();
        assert_eq!(attach.operation, "mcp");
        connected_tx.send(()).unwrap();
        release_rx
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        protocol::write_frame(
            &mut socket,
            &protocol::Reply {
                id: attach.id,
                payload: json!({"result":true,"mcpIdentity":protocol::mcp_identity()}),
            },
        )
        .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut line = String::new();
        let mut steps = Vec::new();
        loop {
            line.clear();
            reader.read_line(&mut line).unwrap();
            let (relay, value) = relayed(&line);
            let method = value["method"].as_str().unwrap();
            steps.push(method.to_owned());
            match method {
                "initialize" => {
                    assert_eq!(value["id"], 1);
                    if fail_setup {
                        break;
                    }
                    writeln!(
                        socket,
                        "{relay} {}",
                        json!({"jsonrpc":"2.0","id":1,"result":{
                            "protocolVersion":"2025-11-25","capabilities":{"tools":{},"prompts":{}},
                            "serverInfo":{"name":"trellis","version":"fixture"}}
                        })
                    )
                    .unwrap();
                }
                "notifications/cancelled" => assert_eq!(value["params"]["requestId"], cancel_id),
                "notifications/initialized" => (),
                "tools/list" => {
                    assert_eq!(value["id"], 5);
                    writeln!(
                        socket,
                        "{relay} {}",
                        json!({"jsonrpc":"2.0","id":5,"result":{"tools":[]}})
                    )
                    .unwrap();
                }
                "tools/call" => {
                    assert_eq!(value["id"], 6);
                    writeln!(
                        socket,
                        "{relay} {}",
                        json!({"jsonrpc":"2.0","id":6,"result":{"isError":false}})
                    )
                    .unwrap();
                    break;
                }
                other => panic!("unexpected synthetic MCP method {other}: {steps:?}"),
            }
        }
        if !fail_setup {
            assert!(
                steps.contains(&"notifications/cancelled".into()),
                "{steps:?}"
            );
        }
        assert_eq!(
            steps
                .iter()
                .filter(|stage| stage.as_str() == "initialize")
                .count(),
            1
        );
        if cancel_id == 5
            && let Some(list) = steps.iter().position(|step| step == "tools/list")
        {
            // Decision 0009 §7 (no relay-local queue): a request may reach
            // the daemon before its cancel is read, but the cancel follows it
            // and the client sees nothing.
            assert!(
                steps[list..].contains(&"notifications/cancelled".to_owned()),
                "canceled ID5 reached the daemon without its cancel: {steps:?}"
            );
        }
    });
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","id":5,"method":"tools/list","params":{}
        })
    )
    .unwrap();
    connected_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":cancel_id}
        })
    )
    .unwrap();
    assert!(
        peer.lines.recv_timeout(Duration::from_millis(150)).is_err(),
        "setup replied while attach paused"
    );
    release_tx.send(()).unwrap();
    if cancel_id == 5 {
        assert!(
            peer.lines.recv_timeout(Duration::from_millis(300)).is_err(),
            "canceled pending ID5 produced stdout"
        );
    } else {
        let list = peer
            .lines
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        assert_eq!(list["id"], 5, "{list}");
        assert!(list["result"]["tools"].is_array(), "{list}");
    }
    let mut fake = Some(fake);
    if fail_setup {
        fake.take().unwrap().join().unwrap();
        drop(owner);
    }
    let call = peer.ask(
        6,
        "tools/call",
        json!({
            "name":"trellis_workspace_describe","arguments":{"schemaVersion":1}
        }),
    );
    assert_eq!(call["id"], 6, "{call}");
    assert_eq!(call["result"]["isError"], false, "{call}");
    if let Some(fake) = fake {
        fake.join().unwrap();
    }
    peer.finish_unreaped();
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(peer.child.wait().unwrap().success());
    peer.reaped = true;
}

#[test]
fn legacy_await_initialized_refuses_tool_after_daemon_death() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = socket_ready(home.path());
    let mut peer = Peer::start(home.path(), &root);
    let init = peer.ask(
        1,
        "initialize",
        json!({
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"regression","version":"1"}
        }),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    let rejected = peer.ask(
        2,
        "tools/call",
        json!({
            "name":"trellis_workspace_describe","arguments":{"schemaVersion":1}
        }),
    );
    assert_eq!(rejected["id"], 2, "{rejected}");
    assert_eq!(rejected["error"]["code"], -32600, "{rejected}");
    assert!(rejected.get("result").is_none(), "{rejected}");
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","method":"notifications/initialized"
        })
    )
    .unwrap();
    let ready = peer.ask(3, "tools/list", json!({}));
    assert_eq!(ready["id"], 3, "{ready}");
    assert!(ready["result"].is_object(), "{ready}");
    peer.finish_unreaped();
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(peer.child.wait().unwrap().success());
    peer.reaped = true;
}

#[test]
fn failed_reattach_drains_pending_cancel_and_stdin_eof_before_failure() {
    for close_stdin in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let root = checkout(home.path());
        let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
            .arg("daemon")
            .env("HOME", home.path())
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .env("XDG_DATA_HOME", home.path().join("data"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let _ = socket_ready(home.path());
        let mut peer = Peer::start(home.path(), &root);
        let meta = json!({"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
            "io.modelcontextprotocol/clientCapabilities":{}}});
        assert_eq!(peer.ask(1, "server/discover", meta.clone())["id"], 1);
        daemon.kill().unwrap();
        daemon.wait().unwrap();
        let mut dirs = vec![home.path().to_path_buf()];
        let mut data = None;
        while let Some(dir) = dirs.pop() {
            for entry in fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.file_name().is_some_and(|name| name == "daemon.lock") {
                    data = path.parent().and_then(Path::parent).map(Path::to_path_buf);
                    break;
                }
                if path.is_dir() {
                    dirs.push(path);
                }
            }
        }
        let owner = trellis::daemon::SocketOwner::acquire(&trellis::daemon::SocketPaths::new(
            &data.unwrap(),
        ))
        .unwrap()
        .unwrap();
        let listener = owner.listener().try_clone().unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let fake = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
                .unwrap();
            let attach: protocol::Request = protocol::read_frame(&mut stream).unwrap();
            assert_eq!(attach.operation, "mcp");
            entered_tx.send(()).unwrap();
            release_rx
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap();
            protocol::write_frame(
                &mut stream,
                &protocol::Reply {
                    id: attach.id,
                    payload: json!({"error":"root_changed"}),
                },
            )
            .unwrap();
        });
        writeln!(
            peer.stdin.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0","id":7,
            "method":"tools/call","params":{"name":"trellis_workspace_describe",
                "arguments":{"schemaVersion":1},"_meta":meta["_meta"]}})
        )
        .unwrap();
        entered_rx
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        if close_stdin {
            peer.stdin.take();
        } else {
            writeln!(
                peer.stdin.as_mut().unwrap(),
                "{}",
                json!({
            "jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":7}})
            )
            .unwrap();
        }
        assert!(
            peer.lines.recv_timeout(Duration::from_millis(150)).is_err(),
            "attach still blocked"
        );
        release_tx.send(()).unwrap();
        fake.join().unwrap();
        if close_stdin {
            peer.finish_unreaped();
            assert!(
                peer.lines.recv_timeout(Duration::from_secs(1)).is_err(),
                "stdin EOF emitted pending failure"
            );
        } else {
            assert!(
                peer.lines.recv_timeout(Duration::from_millis(300)).is_err(),
                "canceled ID7 emitted pending failure"
            );
            drop(owner);
            let recovered = peer.ask(8, "tools/list", meta.clone());
            assert_eq!(recovered["id"], 8, "{recovered}");
            assert!(recovered["result"].is_object(), "{recovered}");
            peer.finish_unreaped();
        }
        unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
        assert!(peer.child.wait().unwrap().success());
        peer.reaped = true;
    }
}

#[test]
fn idle_mcp_connection_outlives_cli_request_deadline() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let mut client = Peer::start(home.path(), &root);
    let metadata = json!({"_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    }});
    assert_eq!(client.ask(1, "server/discover", metadata.clone())["id"], 1);
    // The daemon's 15-second CLI framing deadline must not close an MCP session.
    std::thread::sleep(Duration::from_secs(16));
    assert_eq!(client.ask(2, "tools/list", metadata)["id"], 2);
    client.finish_unreaped();
    unsafe { libc::kill(-(client.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(client.child.wait().unwrap().success());
    client.reaped = true;
}

#[test]
fn partial_daemon_reply_is_typed_and_never_reaches_stdout() {
    use std::io::Read;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    // Start once to create the production private directory, then replace its
    // stale socket under the same singleton lock with a controlled socket peer.
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = socket_ready(home.path());
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    let mut dirs = vec![home.path().to_path_buf()];
    let mut lock = None;
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "daemon.lock") {
                lock = Some(path);
                break;
            }
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    let lock = lock.unwrap();
    let data = lock.parent().unwrap().parent().unwrap();
    let owner = trellis::daemon::SocketOwner::acquire(&trellis::daemon::SocketPaths::new(data))
        .unwrap()
        .expect("the killed daemon released its lock");
    let listener = owner.listener().try_clone().unwrap();
    let fake = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let attach: protocol::Request = protocol::read_frame(&mut stream).unwrap();
        assert_eq!(attach.operation, "mcp");
        protocol::write_frame(
            &mut stream,
            &protocol::Reply {
                id: attach.id,
                payload: json!({"result":true,"mcpIdentity":protocol::mcp_identity()}),
            },
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let (relay, request) = relayed(&line);
        assert_eq!(request["method"], "tools/call");
        stream
            .write_all(format!("{relay} {{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":").as_bytes())
            .unwrap();
        stream.shutdown(std::net::Shutdown::Write).unwrap();
    });
    let mut child = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("mcp")
        .arg("--workspace")
        .arg(&root)
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0", "id":1,"method":"tools/call",
            "params":{"name":"trellis_workspace_describe","arguments":{"schemaVersion":1},
                "_meta":{
                    "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{}
                }}
        })
    )
    .unwrap();
    // Keep stdin open until the interrupted call returns a typed failure.
    // Closing it earlier must suppress that pending response entirely.
    let (tx, lines) = mpsc::sync_channel(2);
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        while reader.read_until(b'\n', &mut line).unwrap_or(0) != 0 {
            assert_reply_cap(&line);
            if tx.send(std::mem::take(&mut line)).is_err() {
                return;
            }
        }
    });
    let first = lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("typed interrupted result while stdin open");
    let response: Value = serde_json::from_slice(&first).unwrap();
    assert_eq!(response["id"], 1);
    assert_eq!(response["result"]["isError"], true, "{response}");
    assert_eq!(response["result"]["structuredContent"]["requestId"], 1);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["code"],
        "workspace_selection_failed"
    );
    assert_eq!(
        response["result"]["structuredContent"]["error"]["reason"],
        "unavailable"
    );
    assert!(
        response["result"]["structuredContent"]
            .get("workspace")
            .is_none()
    );
    assert!(
        response["result"]["structuredContent"]
            .get("catchingUp")
            .is_none()
    );
    let listener = owner.listener().try_clone().unwrap();
    let failed_attach = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let attach: protocol::Request = protocol::read_frame(&mut stream).unwrap();
        assert_eq!(attach.operation, "mcp");
        protocol::write_frame(
            &mut stream,
            &protocol::Reply {
                id: attach.id,
                payload: json!({"error":"root_changed"}),
            },
        )
        .unwrap();
    });
    writeln!(
        child.stdin.as_mut().unwrap(),
        "{}",
        json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"trellis_workspace_describe",
                "arguments":{"schemaVersion":1,"workspace":root.canonicalize().unwrap()},
                "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{}}}
        })
    )
    .unwrap();
    let selected: Value =
        serde_json::from_slice(&lines.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap())
            .unwrap();
    assert_eq!(selected["id"], 2);
    assert_eq!(selected["result"]["isError"], true, "{selected}");
    let envelope = &selected["result"]["structuredContent"];
    assert_eq!(envelope["requestId"], 2);
    assert_eq!(envelope["error"]["code"], "workspace_selection_failed");
    assert_eq!(envelope["error"]["reason"], "unavailable");
    assert_eq!(envelope["error"]["retryable"], true);
    assert_eq!(
        envelope["error"]["attemptedWorkspace"]["value"],
        json!(root.canonicalize().unwrap())
    );
    assert!(
        envelope.get("workspace").is_none(),
        "unverified checkout attributed: {selected}"
    );
    assert!(envelope.get("catchingUp").is_none(), "{selected}");
    let summary = selected["result"]["content"][0]["text"].as_str().unwrap();
    assert_eq!(
        summary,
        "trellis_workspace_describe failed: workspace_selection_failed: Workspace selection failed"
    );
    assert!(summary.len() <= 512);
    failed_attach.join().unwrap();
    // Even though attach now fails, prior modern mode still defines protocol errors.
    let metadata = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    });
    for (id, method, params, code) in [
        (
            3,
            "tools/call",
            json!({"name":"trellis_workspace_describe",
            "arguments":{"schemaVersion":1}}),
            -32602,
        ),
        (
            4,
            "tools/call",
            json!({"name":"trellis_workspace_describe",
            "arguments":{"schemaVersion":1},"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2099-01-01",
                "io.modelcontextprotocol/clientCapabilities":{}}}),
            -32022,
        ),
        (5, "no/such/method", json!({"_meta":metadata}), -32601),
        (
            6,
            "tools/call",
            json!({"name":"unknown_tool","arguments":{},
            "_meta":metadata}),
            -32602,
        ),
    ] {
        writeln!(
            child.stdin.as_mut().unwrap(),
            "{}",
            json!({
                "jsonrpc":"2.0","id":id,"method":method,"params":params
            })
        )
        .unwrap();
        let rejected: Value =
            serde_json::from_slice(&lines.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap())
                .unwrap();
        assert_eq!(rejected["id"], id, "{rejected}");
        assert_eq!(rejected["error"]["code"], code, "{rejected}");
        assert!(rejected.get("result").is_none(), "{rejected}");
        if id == 4 {
            assert_eq!(rejected["error"]["data"]["requested"], "2099-01-01");
        }
    }
    child.stdin.take();
    let deadline = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "client did not finish after stdin EOF"
        );
        std::thread::yield_now();
    };
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .take(65_537)
        .read_to_string(&mut stderr)
        .unwrap();
    fake.join().unwrap();
    owner.listener().set_nonblocking(true).unwrap();
    assert!(
        owner
            .listener()
            .accept()
            .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock),
        "client reattached more than once after the controlled failure"
    );
    assert!(status.success(), "{stderr}");
    assert!(
        lines.recv_timeout(Duration::from_secs(1)).is_err(),
        "partial or duplicate stdout"
    );
}

/// #127: a cancel that arrives after the relay has read the daemon's reply but
/// before it writes that reply must suppress it.
#[test]
fn cancel_after_relay_reads_reply_suppresses_that_reply() {
    use std::os::unix::net::UnixListener;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let phase_path = PathBuf::from(format!(
        "/tmp/trellis-127-relay-{}.sock",
        rand::random::<u64>()
    ));
    let listener = UnixListener::bind(&phase_path).unwrap();
    let mut peer = Peer::start_with_env(
        home.path(),
        &root,
        &[
            ("TRELLIS_TEST_MCP_PHASE", "relay_before_write".as_ref()),
            ("TRELLIS_TEST_MCP_PHASE_SOCKET", phase_path.as_os_str()),
        ],
    );
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    });
    let send = |peer: &mut Peer, frame: Value| {
        writeln!(peer.stdin.as_mut().unwrap(), "{frame}").unwrap();
        peer.stdin.as_mut().unwrap().flush().unwrap();
    };
    let pause = |listener: &UnixListener| {
        let (phase, _) = listener.accept().unwrap();
        phase
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let mut stage = String::new();
        BufReader::new(phase.try_clone().unwrap())
            .read_line(&mut stage)
            .unwrap();
        assert_eq!(stage, "relay_before_write\n");
        phase
    };
    send(
        &mut peer,
        json!({"jsonrpc":"2.0","id":"cancel-A","method":"tools/call",
            "params":{"name":"trellis_workspace_describe",
                "arguments":{"schemaVersion":1},"_meta":meta}}),
    );
    // The relay holds the complete reply for cancel-A and has not written it.
    let mut held = pause(&listener);
    send(
        &mut peer,
        json!({"jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":"cancel-A"}}),
    );
    held.write_all(b"x").unwrap();
    send(
        &mut peer,
        json!({"jsonrpc":"2.0","id":"after","method":"tools/list","params":{"_meta":meta}}),
    );
    pause(&listener).write_all(b"x").unwrap();
    let next = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(next["id"], "after", "cancelled reply was written: {next}");
    let _ = fs::remove_file(&phase_path);
}

/// #127's defined boundary: once the relay has settled every cancel written
/// before its decision point and decided to write, a later cancel is concurrent
/// with the reply. The reply is written once, the late cancel produces no
/// output, and the session continues.
#[test]
fn cancel_after_the_relay_decides_cannot_recall_the_reply() {
    use std::os::unix::net::UnixListener;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let phase_path = PathBuf::from(format!(
        "/tmp/trellis-127-decided-{}.sock",
        rand::random::<u64>()
    ));
    let listener = UnixListener::bind(&phase_path).unwrap();
    let mut peer = Peer::start_with_env(
        home.path(),
        &root,
        &[
            ("TRELLIS_TEST_MCP_PHASE", "relay_after_decision".as_ref()),
            ("TRELLIS_TEST_MCP_PHASE_SOCKET", phase_path.as_os_str()),
        ],
    );
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    });
    let send = |peer: &mut Peer, frame: Value| {
        writeln!(peer.stdin.as_mut().unwrap(), "{frame}").unwrap();
        peer.stdin.as_mut().unwrap().flush().unwrap();
    };
    let pause = |listener: &UnixListener| {
        let (phase, _) = listener.accept().unwrap();
        phase
            .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
            .unwrap();
        let mut stage = String::new();
        BufReader::new(phase.try_clone().unwrap())
            .read_line(&mut stage)
            .unwrap();
        assert_eq!(stage, "relay_after_decision\n");
        phase
    };
    send(
        &mut peer,
        json!({"jsonrpc":"2.0","id":"late-A","method":"tools/call",
            "params":{"name":"trellis_workspace_describe",
                "arguments":{"schemaVersion":1},"_meta":meta}}),
    );
    let mut decided = pause(&listener);
    send(
        &mut peer,
        json!({"jsonrpc":"2.0","method":"notifications/cancelled",
            "params":{"requestId":"late-A"}}),
    );
    decided.write_all(b"x").unwrap();
    let reply = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(reply["id"], "late-A", "{reply}");
    send(
        &mut peer,
        json!({"jsonrpc":"2.0","id":"after","method":"tools/list","params":{"_meta":meta}}),
    );
    pause(&listener).write_all(b"x").unwrap();
    let next = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(
        next["id"], "after",
        "the late cancel produced output: {next}"
    );
    let _ = fs::remove_file(&phase_path);
}

/// Decision 0009 §7: the relay owns the strict client-facing deadline. A call
/// the daemon holds past it is answered deadline_exceeded in the
/// pre-resolution shape; the daemon's late reply is dropped, even when the
/// client has already reused the ID for a new request.
#[test]
fn relay_answers_at_its_deadline_and_drops_the_late_reply() {
    use std::os::unix::net::UnixListener;
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let phase_path = PathBuf::from(format!(
        "/tmp/trellis-138-deadline-{}.sock",
        rand::random::<u64>()
    ));
    let listener = UnixListener::bind(&phase_path).unwrap();
    let mut daemon = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
        .arg("daemon")
        .env("HOME", home.path())
        .env("XDG_CACHE_HOME", home.path().join("cache"))
        .env("XDG_DATA_HOME", home.path().join("data"))
        .env("TRELLIS_TEST_MCP_PHASE", "launch_final")
        .env("TRELLIS_TEST_MCP_PHASE_SOCKET", &phase_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _ = socket_ready(home.path());
    let mut peer = Peer::start_with_env(
        home.path(),
        &root,
        // Long enough that a loaded daemon reaches the hold before the relay
        // decides; the relay must not cancel the call before it is held.
        &[("TRELLIS_TEST_MCP_RELAY_DEADLINE_MS", "2000".as_ref())],
    );
    let meta = json!({
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{}
    });
    let call = json!({"jsonrpc":"2.0","id":"A","method":"tools/call",
        "params":{"name":"trellis_workspace_describe",
            "arguments":{"schemaVersion":1},"_meta":meta}});
    writeln!(peer.stdin.as_mut().unwrap(), "{call}").unwrap();
    // The daemon holds A at its send decision.
    let (mut held, _) = listener.accept().unwrap();
    held.set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let mut stage = String::new();
    BufReader::new(held.try_clone().unwrap())
        .read_line(&mut stage)
        .unwrap();
    assert_eq!(stage, "launch_final\n");
    let reply = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("the relay did not answer at its deadline");
    assert_eq!(reply["id"], "A", "{reply}");
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["error"]["code"], "deadline_exceeded", "{reply}");
    assert_eq!(envelope["error"]["retryable"], true);
    assert_eq!(envelope["requestId"], "A");
    assert!(
        envelope["error"]["attemptedWorkspace"].is_object(),
        "{reply}"
    );
    assert!(envelope.get("workspace").is_none(), "{reply}");
    // The client reuses A at once; then the daemon releases the old A.
    let reuse = json!({"jsonrpc":"2.0","id":"A","method":"tools/list","params":{"_meta":meta}});
    writeln!(peer.stdin.as_mut().unwrap(), "{reuse}").unwrap();
    held.write_all(b"x").unwrap();
    let reused = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(reused["id"], "A", "{reused}");
    assert!(
        reused["result"]["tools"].is_array(),
        "the late reply was written: {reused}"
    );
    assert!(
        peer.lines.recv_timeout(Duration::from_millis(300)).is_err(),
        "a second reply for A was written"
    );
    peer.finish_unreaped();
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    assert!(peer.child.wait().unwrap().success());
    peer.reaped = true;
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    fs::remove_file(&phase_path).unwrap();
}

/// A daemon whose MCP connections pause at `stage` (when set), the phase
/// listener, and its socket path.
fn phase_daemon(
    home: &Path,
    stage: Option<&str>,
) -> (
    std::process::Child,
    std::os::unix::net::UnixListener,
    PathBuf,
) {
    let phase_path = PathBuf::from(format!(
        "/tmp/trellis-138-relay-{}.sock",
        rand::random::<u64>()
    ));
    let listener = std::os::unix::net::UnixListener::bind(&phase_path).unwrap();
    let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    command
        .arg("daemon")
        .env("HOME", home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(stage) = stage {
        command
            .env("TRELLIS_TEST_MCP_PHASE", stage)
            .env("TRELLIS_TEST_MCP_PHASE_SOCKET", &phase_path);
    }
    let daemon = command.spawn().unwrap();
    let _ = socket_ready(home);
    (daemon, listener, phase_path)
}

/// Accept one paused phase connection and check its stage.
fn paused_at(
    listener: &std::os::unix::net::UnixListener,
    stage: &str,
) -> std::os::unix::net::UnixStream {
    try_paused_at(listener, stage)
        .unwrap_or_else(|| panic!("no connection paused at {stage} (hang)"))
}

/// The paused phase connection at `stage`, or `None` if none arrives within
/// the hang backstop: a hold that never comes names its step instead of
/// blocking in `accept` until nextest kills the test (#187).
fn try_paused_at(
    listener: &std::os::unix::net::UnixListener,
    stage: &str,
) -> Option<std::os::unix::net::UnixStream> {
    let accepting = listener.try_clone().unwrap();
    let (accepted, arrival) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = accepted.send(accepting.accept());
    });
    let (phase, _) = arrival
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .ok()?
        .unwrap();
    phase
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let mut seen = String::new();
    BufReader::new(phase.try_clone().unwrap())
        .read_line(&mut seen)
        .unwrap();
    assert_eq!(seen, format!("{stage}\n"));
    Some(phase)
}

fn describe_frame(id: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name":"trellis_workspace_describe","arguments":{"schemaVersion":1},
            "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}}}})
}

fn finish_peer(mut peer: Peer, mut daemon: std::process::Child, phase_path: &Path) {
    peer.finish_unreaped();
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    let _ = peer.child.wait();
    peer.reaped = true;
    daemon.kill().unwrap();
    daemon.wait().unwrap();
    let _ = fs::remove_file(phase_path);
}

/// Decision 0009 §7: the relay decides by each call's own acceptance + 5 s.
/// Held while writing call A's reply past A's deadline, it answers A
/// deadline_exceeded instead of the late success; call B, read meanwhile,
/// expired before it could be forwarded and is decided without the daemon.
#[test]
fn relay_decides_by_acceptance_even_when_held_past_the_deadline() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let (daemon, listener, phase_path) = phase_daemon(home.path(), None);
    // Activate the checkout first, in another session (#187). A cold
    // activation slower than A's 2 s deadline expires A before the daemon
    // answers, so the relay never reaches the hold this test is about.
    // The whole warm-up shares one hang backstop, well under nextest's kill.
    let mut warm = Peer::start(home.path(), &root);
    let until = Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut last = None;
    for attempt in 0.. {
        writeln!(
            warm.stdin.as_mut().unwrap(),
            "{}",
            describe_frame(json!(attempt))
        )
        .unwrap();
        let Ok(reply) = warm
            .lines
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        else {
            panic!("warm-up describe {attempt} was not answered (hang); last reply: {last:?}");
        };
        let envelope = &reply["result"]["structuredContent"];
        if envelope["data"].is_object() {
            break;
        }
        // Only the activation outlasting the call deadline is retried.
        assert_eq!(envelope["error"]["code"], "deadline_exceeded", "{reply}");
        assert!(
            Instant::now() < until,
            "warm-up activation never answered describe (hang); last reply: {reply}"
        );
        last = Some(reply);
    }
    drop(warm);
    let deadline = Duration::from_millis(2000);
    let mut peer = Peer::start_with_env(
        home.path(),
        &root,
        &[
            ("TRELLIS_TEST_MCP_RELAY_DEADLINE_MS", "2000".as_ref()),
            ("TRELLIS_TEST_MCP_PHASE", "relay_before_write".as_ref()),
            ("TRELLIS_TEST_MCP_PHASE_SOCKET", phase_path.as_os_str()),
        ],
    );
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        describe_frame(json!("A"))
    )
    .unwrap();
    // The daemon answered A; the relay is held before deciding it.
    let Some(mut held) = try_paused_at(&listener, "relay_before_write") else {
        let decided: Vec<Value> = peer.lines.try_iter().collect();
        panic!("the relay never held A's daemon reply; decided without it: {decided:?}");
    };
    let written_b = Instant::now();
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        describe_frame(json!("B"))
    )
    .unwrap();
    // Past A's deadline (A was accepted before the hold) and past B's,
    // allowing B's stdin read ample time to complete.
    std::thread::sleep(
        (written_b + deadline + Duration::from_millis(700))
            .saturating_duration_since(Instant::now()),
    );
    held.write_all(b"x").unwrap();
    for id in ["A", "B"] {
        let reply = peer
            .lines
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        assert_eq!(reply["id"], id, "{reply}");
        let envelope = &reply["result"]["structuredContent"];
        assert_eq!(envelope["error"]["code"], "deadline_exceeded", "{reply}");
        assert!(envelope.get("workspace").is_none(), "{reply}");
    }
    finish_peer(peer, daemon, &phase_path);
}

/// #127 per ID: an immediate refusal (the fifth call) is a decided outcome
/// too, so a cancel the client wrote before it suppresses it.
#[test]
fn a_cancel_written_with_a_fifth_call_suppresses_its_refusal() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let (daemon, listener, phase_path) = phase_daemon(home.path(), Some("launch_final"));
    let mut peer = Peer::start(home.path(), &root);
    // Activate the checkout in this session first: the four timed calls must
    // not also race a cold activation against their deadline.
    writeln!(peer.stdin.as_mut().unwrap(), "{}", describe_frame(json!(0))).unwrap();
    paused_at(&listener, "launch_final")
        .write_all(b"x")
        .unwrap();
    let warm = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(warm["id"], 0, "{warm}");
    assert!(
        warm["result"]["structuredContent"]["data"].is_object(),
        "{warm}"
    );
    for id in 1..=4 {
        writeln!(
            peer.stdin.as_mut().unwrap(),
            "{}",
            describe_frame(json!(id))
        )
        .unwrap();
    }
    // Four calls are outstanding while the daemon holds the first.
    let first = paused_at(&listener, "launch_final");
    let burst = format!(
        "{}\n{}\n",
        describe_frame(json!(5)),
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":5}})
    );
    peer.stdin
        .as_mut()
        .unwrap()
        .write_all(burst.as_bytes())
        .unwrap();
    // Only the first hold matters: the burst arrives while the relay has four
    // live calls. Later pauses are released at once, so the remaining calls
    // are not serialized through this thread against their deadlines.
    let later = listener.try_clone().unwrap();
    let releaser = std::thread::spawn(move || {
        for _ in 0..3 {
            paused_at(&later, "launch_final").write_all(b"x").unwrap();
        }
    });
    let mut first = first;
    first.write_all(b"x").unwrap();
    let mut answered = Vec::new();
    while answered.len() < 4 {
        let reply = peer
            .lines
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        assert_ne!(reply["id"], 5, "the cancelled refusal was written: {reply}");
        assert!(
            reply["result"]["structuredContent"]["data"].is_object(),
            "{reply}"
        );
        answered.push(reply["id"].as_i64().unwrap());
    }
    releaser.join().unwrap();
    answered.sort_unstable();
    assert_eq!(answered, vec![1, 2, 3, 4]);
    assert!(
        peer.lines.recv_timeout(Duration::from_millis(300)).is_err(),
        "a reply for the cancelled fifth call"
    );
    finish_peer(peer, daemon, &phase_path);
}

/// Decision 0009 §7: after the relay answers a call at its deadline and the
/// client reuses the ID for a new call while the daemon's old worker is
/// still running, the new call is admitted and answered; the old worker's
/// late outcome never reaches the client.
#[test]
fn a_reused_call_id_is_answered_while_the_old_worker_still_runs() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let (daemon, listener, phase_path) = phase_daemon(home.path(), Some("call_worker"));
    let mut peer = Peer::start_with_env(
        home.path(),
        &root,
        &[("TRELLIS_TEST_MCP_RELAY_DEADLINE_MS", "2000".as_ref())],
    );
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        describe_frame(json!("A"))
    )
    .unwrap();
    let mut old_worker = paused_at(&listener, "call_worker");
    let expired = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(expired["id"], "A", "{expired}");
    assert_eq!(
        expired["result"]["structuredContent"]["error"]["code"], "deadline_exceeded",
        "{expired}"
    );
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        describe_frame(json!("A"))
    )
    .unwrap();
    paused_at(&listener, "call_worker").write_all(b"x").unwrap();
    let reused = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(reused["id"], "A", "{reused}");
    assert!(
        reused["result"]["structuredContent"]["data"].is_object(),
        "the reused call was refused or decided by the old one: {reused}"
    );
    old_worker.write_all(b"x").unwrap();
    assert!(
        peer.lines.recv_timeout(Duration::from_millis(300)).is_err(),
        "the old worker's outcome reached the client"
    );
    finish_peer(peer, daemon, &phase_path);
}

/// Decision 0009 §7: the relay checks a call's deadline at its send
/// decision, after settling input. A timely daemon reply whose settling ends
/// past the deadline is answered deadline_exceeded, not the success.
#[test]
fn a_reply_settled_past_the_deadline_is_not_a_success() {
    let home = tempfile::tempdir().unwrap();
    let root = checkout(home.path());
    let (daemon, listener, phase_path) = phase_daemon(home.path(), None);
    let mut peer = Peer::start_with_env(
        home.path(),
        &root,
        &[
            ("TRELLIS_TEST_MCP_RELAY_DEADLINE_MS", "2000".as_ref()),
            ("TRELLIS_TEST_MCP_PHASE", "relay_after_settle".as_ref()),
            ("TRELLIS_TEST_MCP_PHASE_SOCKET", phase_path.as_os_str()),
        ],
    );
    let written = Instant::now();
    writeln!(
        peer.stdin.as_mut().unwrap(),
        "{}",
        describe_frame(json!("A"))
    )
    .unwrap();
    // The daemon answered in time and the relay settled; held before the
    // send decision until A's deadline (from its acceptance) has passed.
    let mut held = paused_at(&listener, "relay_after_settle");
    std::thread::sleep(
        (written + Duration::from_millis(2000) + Duration::from_millis(500))
            .saturating_duration_since(Instant::now()),
    );
    held.write_all(b"x").unwrap();
    let reply = peer
        .lines
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(reply["id"], "A", "{reply}");
    assert_eq!(
        reply["result"]["structuredContent"]["error"]["code"], "deadline_exceeded",
        "a success was sent past its deadline: {reply}"
    );
    finish_peer(peer, daemon, &phase_path);
}
