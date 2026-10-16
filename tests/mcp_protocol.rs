#![cfg(feature = "test-support")]
//! Executable stdio contract checks. Requests wait for replies; no timing-dependent queue hooks.
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::Duration,
};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/mcp/protocol.json")).unwrap()
}
/// Single-owner waitid(WNOWAIT) observes exit without reaping. The original PID/PGID
/// cannot be recycled until *after* this owner terminates the group and reaps it.
fn bounded_group_wait(child: &mut Child) -> (std::process::ExitStatus, bool) {
    let pid = child.id() as libc::pid_t;
    // Both spawn sites use CommandExt::process_group(0); a failed group setup
    // fails spawn. Do not probe getpgid here: Darwin can transiently report
    // ESRCH before a nonblocking waitid observes a fast child's exit.
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let expired = loop {
        if std::time::Instant::now() >= deadline {
            break true;
        }
        let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
        let status = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                info.as_mut_ptr(),
                libc::WEXITED | libc::WNOWAIT | libc::WNOHANG,
            )
        };
        assert_eq!(
            status,
            0,
            "non-reaping waitid failed: {}",
            std::io::Error::last_os_error()
        );
        if unsafe { info.assume_init().si_pid() } == pid {
            // A late scheduling resume must not make an over-deadline exit valid.
            break std::time::Instant::now() >= deadline;
        }
        std::thread::yield_now();
    };
    // This owner has never reaped the leader. Even if it exited, its PID and
    // process-group identity remain reserved until child.wait() below.
    unsafe { libc::kill(-pid, libc::SIGKILL) };
    let status = child.wait().unwrap();
    (status, expired)
}
fn capture_file(path: &Path) -> Vec<u8> {
    let file = fs::File::open(path).unwrap();
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes).unwrap();
    assert!(
        bytes.len() <= 65_536,
        "subprocess output exceeded bounded capture"
    );
    bytes
}
/// Silence is established only after the reader has closed its sender. Timeout is
/// inconclusive and must fail, never count as an empty protocol transcript.
fn drain_stdout(lines: Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut captured = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "stdout reader did not reach EOF within 10 seconds"
        );
        match lines.recv_timeout(remaining) {
            Ok(line) => {
                assert!(line.len() <= 65_536, "stdout frame exceeds response cap");
                captured.push(line);
                assert!(captured.len() <= 16, "too many stdout frames after EOF");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return captured,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("stdout reader did not reach EOF within 10 seconds")
            }
        }
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

fn private_socket_under(home: &Path) -> Option<PathBuf> {
    let mut dirs = vec![home.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.file_name().is_some_and(|name| name == "daemon.lock") {
                return Some(trellis::daemon::SocketPaths::new(path.parent()?.parent()?).socket);
            }
            if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    None
}

fn owned_daemon_connection(home: &Path, pid: u32) -> Option<std::os::unix::net::UnixStream> {
    let stream = std::os::unix::net::UnixStream::connect(private_socket_under(home)?).ok()?;
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

/// Signal only the elected binary that owns THIS fixture's private socket.
/// Leave a stale or ambiguous marker untouched rather than targeting a reused PID.
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
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    while std::time::Instant::now() < deadline {
        if owned_daemon_connection(home, pid).is_none() {
            let _ = fs::remove_file(&marker);
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // No numeric-PID SIGKILL: a disconnected/replaced socket makes it ambiguous.
    drop(connection);
}

struct Peer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Vec<u8>>,
    stderr_path: PathBuf,
    reaped: bool,
    _home: tempfile::TempDir,
}
impl Peer {
    fn start(workspace: Option<&Path>) -> Self {
        Self::start_in(workspace, None)
    }
    fn start_in(workspace: Option<&Path>, cwd: Option<&Path>) -> Self {
        Self::start_with_phase(workspace, cwd, None)
    }
    fn start_with_phase(
        workspace: Option<&Path>,
        cwd: Option<&Path>,
        phase: Option<(&str, &Path)>,
    ) -> Self {
        Self::start_with_fixture_home(tempfile::tempdir().unwrap(), workspace, cwd, phase, None)
    }
    fn start_with_causal_witness(workspace: Option<&Path>) -> (Self, tokio::net::UnixListener) {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        fs::set_permissions(home.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let socket = home.path().join("mcp-causal.sock");
        assert!(socket.is_absolute());
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            fs::metadata(home.path()).unwrap().permissions().mode() & 0o077,
            0
        );
        let peer = Self::start_with_fixture_home(home, workspace, None, None, Some(&socket));
        (peer, listener)
    }
    fn start_with_fixture_home(
        home: tempfile::TempDir,
        workspace: Option<&Path>,
        cwd: Option<&Path>,
        phase: Option<(&str, &Path)>,
        causal_socket: Option<&Path>,
    ) -> Self {
        let stderr_path = home.path().join("mcp-stderr.log");
        let stderr = fs::File::create(&stderr_path).unwrap();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_trellis"));
        cmd.arg("mcp")
            .current_dir(cwd.unwrap_or(home.path()))
            .env("HOME", home.path())
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env(
                "TRELLIS_TEST_DAEMON_PID_FILE",
                home.path().join("auto-daemon.pid"),
            )
            .process_group(0)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(stderr));
        if let Some(path) = workspace {
            cmd.arg("--workspace").arg(path);
        }
        if let Some((stage, socket)) = phase {
            cmd.env("TRELLIS_TEST_MCP_PHASE", stage)
                .env("TRELLIS_TEST_MCP_PHASE_SOCKET", socket);
        }
        if let Some(socket) = causal_socket {
            cmd.env("TRELLIS_TEST_MCP_CAUSAL_SOCKET", socket);
        }
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::sync_channel(16);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                let available = reader.fill_buf().unwrap();
                if available.is_empty() {
                    if !line.is_empty() {
                        let _ = tx.send(line);
                    }
                    break;
                }
                let end = available.iter().position(|&b| b == b'\n');
                let take = end.map_or(available.len(), |at| at + 1);
                if line.len() + take > 65_536 {
                    // Treat an unbounded line as a failed protocol response, never
                    // allocate an attacker-controlled amount of captured stdout.
                    let _ = tx.send(vec![0; 65_537]);
                    break;
                }
                line.extend_from_slice(&available[..take]);
                reader.consume(take);
                if end.is_some() && tx.send(std::mem::take(&mut line)).is_err() {
                    break;
                }
            }
        });
        Self {
            stdin: Some(child.stdin.take().unwrap()),
            child,
            lines: rx,
            stderr_path,
            reaped: false,
            _home: home,
        }
    }
    fn send_raw(&mut self, line: &[u8]) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
    }
    fn ask(&mut self, value: Value) -> Value {
        self.send_raw(serde_json::to_string(&value).unwrap().as_bytes());
        self.reply()
    }
    fn reply(&self) -> Value {
        serde_json::from_slice(&self.reply_bytes()).unwrap()
    }
    fn reply_bytes(&self) -> Vec<u8> {
        let raw = self
            .lines
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .expect("bounded MCP reply");
        assert!(raw.len() <= 65_536);
        assert_eq!(raw.last(), Some(&b'\n'));
        assert!(
            !raw[..raw.len() - 1].contains(&b'\n'),
            "one JSON message per line"
        );
        raw
    }
    fn finish(mut self) -> (std::process::ExitStatus, String) {
        self.stdin.take();
        stop_fixture_daemon(self._home.path());
        let (status, expired) = bounded_group_wait(&mut self.child);
        self.reaped = true;
        assert!(!expired, "MCP subprocess exceeded 10-second deadline");
        let stderr = String::from_utf8_lossy(&capture_file(&self.stderr_path)).into_owned();
        (status, stderr)
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        // Also covers assertion failure before finish(): only this unique HOME's
        // elected daemon can have written its opt-in marker.
        stop_fixture_daemon(self._home.path());
        if !self.reaped {
            // No previous wait/reap occurred: this PID/PGID cannot be recycled.
            unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
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
fn marker_for_other_fixture_daemon_does_not_target_this_private_socket() {
    let home = tempfile::tempdir().unwrap();
    let owner = trellis::daemon::SocketOwner::acquire(&trellis::daemon::SocketPaths::new(
        &home.path().join("private-data"),
    ))
    .unwrap()
    .unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut decoy = Command::new(env!("CARGO_BIN_EXE_trellis"))
        .arg("daemon")
        .env("HOME", other.path())
        .env("XDG_CACHE_HOME", other.path().join("cache"))
        .env("XDG_DATA_HOME", other.path().join("data"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if private_socket_under(other.path())
            .is_some_and(|path| std::os::unix::net::UnixStream::connect(path).is_ok())
        {
            break;
        }
        assert!(decoy.try_wait().unwrap().is_none());
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    fs::write(home.path().join("auto-daemon.pid"), decoy.id().to_string()).unwrap();
    stop_fixture_daemon(home.path());
    let alive = decoy.try_wait().unwrap().is_none();
    let _ = decoy.kill();
    let _ = decoy.wait();
    drop(owner);
    assert!(alive, "a different fixture's elected daemon was signaled");
}

fn checkout(base: &Path) -> PathBuf {
    let path = base.join("checkout");
    fs::create_dir_all(path.join(".git")).unwrap();
    path
}
fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
fn modern(id: Value, method: &str, fields: Value) -> Value {
    let mut p = fields.as_object().unwrap().clone();
    p.insert("_meta".into(), fixture()["modernMeta"].clone());
    request(id, method, Value::Object(p))
}
fn error(v: &Value, code: i64, id: Value) {
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], id);
    assert_eq!(v["error"]["code"], code, "{v}");
}
fn tool(v: &Value, code: Option<&str>, modern: bool) {
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["result"]["resultType"].as_str().is_some(), modern);
    assert_eq!(v["result"]["isError"], code.is_some(), "{v}");
    let envelope = &v["result"]["structuredContent"];
    assert_eq!(
        serde_json::from_str::<Value>(v["result"]["content"][0]["text"].as_str().unwrap()).unwrap(),
        *envelope
    );
    assert_eq!(envelope["requestId"], v["id"]);
    if let Some(code) = code {
        // The whole reply, so an unexpected code shows its reason.
        assert_eq!(envelope["error"]["code"], code, "{v}");
    }
}
fn expected_envelope(id: &Value, code: Option<&str>, label: &str, root: &Value) -> Value {
    let fixture = fixture();
    if let Some(code) = code {
        let spec = &fixture["expected"]["failures"][code];
        assert!(spec.is_array(), "missing golden error {code}");
        let mut envelope = json!({"schemaVersion":1,"requestId":id,"error":{"code":code,
            "message":spec[0],"retryable":spec[1],"currentBasis":null,"currentContentHash":null}});
        if code != "invalid_request" && code != "range_too_large" {
            envelope["workspace"] = root.clone();
            envelope["catchingUp"] = json!(true);
        }
        envelope
    } else {
        let mut envelope = fixture["expected"]["describe"].clone();
        envelope["requestId"] = id.clone();
        envelope["data"]["workspaceLabel"] = json!(label);
        envelope["workspace"] = root.clone();
        envelope["catchingUp"] = json!(true);
        envelope
    }
}
fn exact_tool(v: &Value, code: Option<&str>, modern: bool, label: &str) {
    let root = &v["result"]["structuredContent"]["workspace"];
    let mut envelope = expected_envelope(&v["id"], code, label, root);
    if code.is_none() {
        let actual = &v["result"]["structuredContent"];
        let state = actual["data"]["indexState"].as_str().unwrap();
        assert!(matches!(state, "indexing" | "ready"), "{actual}");
        envelope["data"]["indexState"] = json!(state);
        envelope["catchingUp"] = actual["catchingUp"].clone();
        assert!(actual["catchingUp"].is_boolean());
        if state == "ready" {
            let languages = actual["data"]["languages"].as_array().unwrap();
            assert_eq!(languages.len(), 4);
            for (row, language) in languages
                .iter()
                .zip(["java", "rust", "python", "javascript"])
            {
                assert_eq!(row["language"], language);
                let coverage = &row["coverage"];
                let count = |field: &str| coverage[field].as_u64().unwrap();
                assert_eq!(
                    count("selected"),
                    count("complete") + count("partial") + count("failed")
                );
                assert!(row["evidenceTier"] == "syntax");
            }
            let partial = languages
                .iter()
                .any(|row| row["coverage"]["partial"] != 0 || row["coverage"]["failed"] != 0);
            assert_eq!(actual["partial"], partial);
            assert!(actual["evidenceBasis"]["indexRevision"].as_u64().unwrap() > 0);
            envelope["evidenceBasis"] = actual["evidenceBasis"].clone();
            envelope["data"]["progress"] = Value::Null;
            envelope["data"]["languages"] = actual["data"]["languages"].clone();
            envelope["partial"] = json!(partial);
        } else {
            assert!(actual["evidenceBasis"].is_null());
            assert_eq!(actual["data"]["languages"], json!([]));
            assert!(
                actual["data"]["progress"].is_null()
                    || actual["data"]["progress"] == json!({"completed":0,"total":null})
            );
            envelope["data"]["progress"] = actual["data"]["progress"].clone();
        }
    }
    let text = serde_json::to_string(&envelope).unwrap();
    let mut result = json!({"isError":code.is_some(),"structuredContent":envelope,
        "content":[{"type":"text","text":text}]});
    if modern {
        result["resultType"] = json!("complete");
    }
    assert_eq!(*v, json!({"jsonrpc":"2.0","id":v["id"],"result":result}));
}
fn exact_selection_failure(v: &Value, id: i64, reason: &str, attempted: &Path) {
    let envelope = json!({"schemaVersion":1,"requestId":id,"error":{
        "code":"workspace_selection_failed","message":"Workspace selection failed",
        "retryable":reason == "unavailable","reason":reason,
        "attemptedWorkspace":{"kind":"path","value":attempted,"utf8Bytes":attempted.to_str().unwrap().len(),"truncated":false},
        "currentBasis":null,"currentContentHash":null}});
    let result = json!({"resultType":"complete","isError":true,
        "structuredContent":envelope,
        "content":[{"type":"text","text":serde_json::to_string(&envelope).unwrap()}]});
    assert_eq!(*v, json!({"jsonrpc":"2.0","id":id,"result":result}));
}
fn exact_tool_for_id(
    v: &Value,
    expected_id: &Value,
    code: Option<&str>,
    modern: bool,
    label: &str,
) {
    assert_eq!(&v["id"], expected_id, "outer JSON-RPC ID must echo sent ID");
    assert_eq!(
        &v["result"]["structuredContent"]["requestId"], expected_id,
        "typed requestId must echo sent ID"
    );
    exact_tool(v, code, modern, label);
}
/// Exact post-H golden responses include the separately serialized text copy.
fn exact_golden_settled(v: &Value, expected_id: &Value, modern: bool, envelope: Value) {
    assert_eq!(&v["id"], expected_id, "outer JSON-RPC ID must echo sent ID");
    assert_eq!(envelope["requestId"], *expected_id);
    let text = serde_json::to_string(&envelope).unwrap();
    let mut result = json!({"isError":envelope.get("error").is_some(),
        "structuredContent":envelope,"content":[{"type":"text","text":text}]});
    if modern {
        result["resultType"] = json!("complete");
    }
    assert_eq!(
        *v,
        json!({"jsonrpc":"2.0","id":expected_id,"result":result})
    );
}

fn golden_settled_failure(id: &Value, root: &Value, code: &str, basis: Value) -> Value {
    let (message, retryable) = match code {
        "not_found" => ("Not found", false),
        "revision_conflict" => ("Revision conflict", false),
        "index_not_ready" => ("Index is not ready", true),
        _ => panic!("unexpected post-H golden failure {code}"),
    };
    json!({"schemaVersion":1,"requestId":id,"workspace":root,
        "catchingUp":false,"error":{"code":code,"message":message,
            "retryable":retryable,"currentBasis":basis,"currentContentHash":null}})
}

fn golden_settled_empty_find(id: &Value, root: &Value, pin: &Value) -> Value {
    json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":false,
        "evidenceBasis":pin,"data":{"symbols":[],"coverage":[],"provenance":[]},
        "warnings":[{"code":"syntaxOnly","message":"Only syntax evidence is available",
            "provenanceId":null}],"partial":false,"truncated":false,
        "truncationReason":null})
}

fn ready_legacy(peer: &mut Peer) {
    let init = peer.ask(request(
        json!(1),
        "initialize",
        fixture()["legacyInitialize"].clone(),
    ));
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    peer.send_raw(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
}
fn call(legacy: bool, id: Value, name: &str, arguments: Value) -> Value {
    let fields = json!({"name":name,"arguments":arguments});
    if legacy {
        request(id, "tools/call", fields)
    } else {
        modern(id, "tools/call", fields)
    }
}
fn phase_barrier(listener: &std::os::unix::net::UnixListener, stage: &str, mutate: impl FnOnce()) {
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::yield_now()
            }
            Err(error) => panic!("MCP phase {stage} was not reached: {error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, format!("{stage}\n"));
    mutate();
    stream.write_all(b"x").unwrap();
}

fn no_db_under(root: &Path) {
    no_db_under_except(root, &[]);
}
fn no_db_under_except(root: &Path, allowed: &[PathBuf]) {
    fn walk(dir: &Path, allowed: &[PathBuf]) {
        assert!(
            dir.is_dir(),
            "isolated HOME must exist during DB inspection: {}",
            dir.display()
        );
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(&path, allowed)
            } else {
                assert!(
                    !matches!(
                        entry.file_name().to_str(),
                        Some("index.db" | "workspace.db" | "requests.db")
                    ) || allowed.contains(&path),
                    "unexpected DB: {}",
                    path.display()
                );
            }
        }
    }
    walk(root, allowed);
}

#[test]
fn both_modes_full_catalog_tools_and_lifecycle() {
    for legacy in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let root = checkout(tmp.path());
        let mut peer = Peer::start(Some(&root));
        if legacy {
            let init = peer.ask(request(
                json!(1),
                "initialize",
                fixture()["legacyInitialize"].clone(),
            ));
            assert_eq!(
                init["result"],
                json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}},"serverInfo":{"name":"trellis","version":env!("CARGO_PKG_VERSION")}})
            );
            error(
                &peer.ask(request(json!(2), "tools/list", json!({}))),
                -32600,
                json!(2),
            );
            peer.send_raw(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
        } else {
            let discover = peer.ask(modern(json!(1), "server/discover", json!({})));
            assert_eq!(
                discover["result"],
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","supportedVersions":["2026-07-28","2025-11-25"],"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"trellis","version":env!("CARGO_PKG_VERSION")}}})
            );
        }
        let list = if legacy {
            request(json!("catalog"), "tools/list", json!({}))
        } else {
            modern(json!("catalog"), "tools/list", json!({}))
        };
        let response = peer.ask(list);
        let catalog: Value =
            serde_json::from_str(include_str!("fixtures/mcp/catalog.json")).unwrap();
        assert_eq!(catalog.as_array().unwrap().len(), 4);
        let expected_list = |id: Value| {
            let result = if legacy {
                json!({"tools":catalog})
            } else {
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","tools":catalog})
            };
            json!({"jsonrpc":"2.0","id":id,"result":result})
        };
        assert_eq!(
            response,
            expected_list(json!("catalog")),
            "entire catalog wrapper must be exact"
        );
        let max_control_id = format!(
            "{}{}{}{}",
            "\"".repeat(80),
            "\\".repeat(20),
            "\u{0000}".repeat(7),
            "a".repeat(12)
        );
        assert_eq!(2 + 80 * 2 + 20 * 2 + 7 * 6 + 12, 256);
        for max_id in ["a".repeat(254), max_control_id] {
            let max_list = if legacy {
                request(json!(max_id), "tools/list", json!({}))
            } else {
                modern(json!(max_id), "tools/list", json!({}))
            };
            let max_reply = peer.ask(max_list);
            assert_eq!(
                max_reply,
                expected_list(json!(max_id)),
                "max-ID entire catalog wrapper must be exact"
            );
        }
        let names = &fixture()["names"];
        for (i, name) in names.as_array().unwrap().iter().enumerate() {
            let fields = json!({"name":name,"arguments":fixture()["validArguments"][i]});
            let call = if legacy {
                request(json!(i + 20), "tools/call", fields)
            } else {
                modern(json!(i + 20), "tools/call", fields)
            };
            let reply = peer.ask(call);
            tool(
                &reply,
                if i == 0 {
                    None
                } else {
                    Some("index_not_ready")
                },
                !legacy,
            );
            if i == 0 {
                exact_tool(&reply, None, !legacy, "checkout");
                assert_eq!(
                    reply["result"]["structuredContent"]["data"]["workspaceLabel"],
                    "checkout"
                );
            }
        }
        let control_id = format!("{}aa", "\u{0000}".repeat(42));
        let fields = json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}});
        let max_call = if legacy {
            request(json!(control_id), "tools/call", fields)
        } else {
            modern(json!(control_id), "tools/call", fields)
        };
        let max_response = peer.ask(max_call);
        tool(&max_response, None, !legacy);
        assert_eq!(max_response["id"], control_id);
        let fields = json!({"name":"trellis_find_symbols","arguments":{"schemaVersion":1,"query":"a","limit":51}});
        let reply = peer.ask(if legacy {
            request(json!(40), "tools/call", fields)
        } else {
            modern(json!(40), "tools/call", fields)
        });
        tool(&reply, Some("range_too_large"), !legacy);
        let wrong = json!({"name":"unknown","arguments":{}});
        error(
            &peer.ask(if legacy {
                request(json!(41), "tools/call", wrong)
            } else {
                modern(json!(41), "tools/call", wrong)
            }),
            -32602,
            json!(41),
        );
        let opposite = if legacy {
            modern(json!(42), "server/discover", json!({}))
        } else {
            request(
                json!(42),
                "initialize",
                fixture()["legacyInitialize"].clone(),
            )
        };
        error(&peer.ask(opposite), -32600, json!(42));
        assert!(peer.finish().0.success());
    }
}
#[test]
fn framing_ids_negotiation_and_application_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    let mut peer = Peer::start(Some(&root));
    peer.send_raw(b"[1]");
    error(&peer.reply(), -32600, Value::Null);
    peer.send_raw(b"{not json}");
    error(&peer.reply(), -32700, Value::Null);
    peer.send_raw(&vec![b'x'; 16_384]);
    error(&peer.reply(), -32700, Value::Null);
    peer.send_raw(&vec![b'x'; 16_385]);
    error(&peer.reply(), -32600, Value::Null);
    for (unit, repeats, tail) in [
        ("a", 254, 0),
        ("\"", 127, 0),
        ("\\", 127, 0),
        ("\u{0000}", 42, 2),
        ("😀", 63, 2),
        ("é", 127, 0),
    ] {
        let good = format!("{}{}", unit.repeat(repeats), "a".repeat(tail));
        let invalid = format!("{good}a");
        peer.send_raw(
            serde_json::to_string(&modern(json!(invalid), "server/discover", json!({})))
                .unwrap()
                .as_bytes(),
        );
        error(&peer.reply(), -32600, Value::Null);
        let valid = peer.ask(modern(json!(good.clone()), "server/discover", json!({})));
        assert_eq!(valid["id"], good);
    }
    for spelling in [r#""\u00e9""#, r#""é""#, r#""\ud83d\ude00""#, r#""😀""#] {
        peer.send_raw(format!(r#"{{"jsonrpc":"2.0","id":{spelling},"method":"tools/list","params":{{"_meta":{}}}}}"#,fixture()["modernMeta"]).as_bytes());
        let v = peer.reply();
        assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 4);
    }
    for n in ["1e0", "1.0", "-0", "9007199254740991", "-9007199254740991"] {
        peer.send_raw(format!(r#"{{"jsonrpc":"2.0","id":{n},"method":"server/discover","params":{{"_meta":{}}}}}"#,fixture()["modernMeta"]).as_bytes());
        let v = peer.reply();
        assert_eq!(
            v["id"],
            serde_json::from_str::<Value>(n)
                .unwrap()
                .as_i64()
                .unwrap_or(1)
        );
    }
    peer.send_raw(format!(r#"{{"jsonrpc":"2.0","id":9007199254740992,"method":"tools/list","params":{{"_meta":{}}}}}"#,fixture()["modernMeta"]).as_bytes());
    error(&peer.reply(), -32600, Value::Null);
    let mut bad = fixture()["modernMeta"].clone();
    bad["io.modelcontextprotocol/clientInfo"] = json!({"name":"x","version":"1","title":42});
    error(
        &peer.ask(request(json!(88), "tools/list", json!({"_meta":bad}))),
        -32602,
        json!(88),
    );
    let mut extension = fixture()["modernMeta"].clone();
    extension["io.modelcontextprotocol/clientInfo"] =
        json!({"name":"x","version":"1","title":"good","futureField":true});
    assert!(
        peer.ask(request(json!(89), "tools/list", json!({"_meta":extension})))["result"]["tools"]
            .is_array()
    );
    let mut version = fixture()["modernMeta"].clone();
    version["io.modelcontextprotocol/protocolVersion"] = json!("2099-01-01");
    let unsupported = peer.ask(request(json!(90), "tools/list", json!({"_meta":version})));
    error(&unsupported, -32022, json!(90));
    assert_eq!(
        unsupported["error"]["data"],
        json!({"supported":["2026-07-28","2025-11-25"],"requested":"2099-01-01"})
    );
    for (name, wrong, code) in [
        (
            "trellis_workspace_describe",
            json!({"schemaVersion":1,"extra":0}),
            "invalid_request",
        ),
        (
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":""}),
            "invalid_request",
        ),
        (
            "trellis_inspect",
            json!({"schemaVersion":1,"symbolId":"sid:v1:x","view":"declaration"}),
            "invalid_request",
        ),
        (
            "trellis_read_source",
            json!({"schemaVersion":1,"path":"../outside","startLine":1,"endLine":1}),
            "invalid_request",
        ),
        (
            "trellis_read_source",
            json!({"schemaVersion":1,"path":"a","startLine":1,"endLine":201}),
            "range_too_large",
        ),
    ] {
        tool(
            &peer.ask(modern(
                json!(91),
                "tools/call",
                json!({"name":name,"arguments":wrong}),
            )),
            Some(code),
            true,
        );
    }
    assert!(peer.finish().0.success());
}
#[test]
fn legacy_title_recovery_max_id_and_root_selection() {
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    let mut peer = Peer::start(Some(&root));
    let mut init = fixture()["legacyInitialize"].clone();
    init["clientInfo"]["title"] = json!(42);
    error(
        &peer.ask(request(json!(7), "initialize", init)),
        -32602,
        json!(7),
    );
    let mut valid = fixture()["legacyInitialize"].clone();
    valid["_meta"] = json!({"progressToken":9007199254740992_u64});
    valid["clientInfo"]["futureField"] = json!(true);
    assert_eq!(
        peer.ask(request(json!(8), "initialize", valid))["result"]["protocolVersion"],
        "2025-11-25"
    );
    peer.send_raw(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert_eq!(
        peer.ask(request(
            json!(9007199254740991_i64),
            "tools/list",
            json!({})
        ))["result"]["tools"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert!(peer.finish().0.success());
    // Topology overlap is refused before marker attachment.
    let overlap = tmp.path().join("managed-home");
    fs::create_dir_all(overlap.join(".git")).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_trellis"));
    command
        .arg("mcp")
        .arg("--workspace")
        .arg(&overlap)
        .env("HOME", &overlap)
        .env("XDG_CACHE_HOME", overlap.join("cache"))
        .env("XDG_DATA_HOME", overlap.join("data"));
    let outcome = bounded_output(command);
    assert!(!outcome.status.success());
    assert!(
        outcome.status.signal().is_none(),
        "overlap refusal must exit naturally, not from a signal"
    );
    assert!(outcome.stdout.is_empty());
    assert!(!overlap.join(".git/trellis/workspace-id").exists());
    assert!(root.join(".git/trellis/workspace-id").is_file());
    // A separate explicit non-Git workspace cannot attach to the daemon.
    let no_git = tmp.path().join("not-checkout");
    fs::create_dir(&no_git).unwrap();
    silent_failure(Peer::start(Some(&no_git)));
    assert!(!no_git.join(".git").exists());
}
#[test]
fn eof_and_cancellation_are_silent_for_no_pending_request() {
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    let mut peer = Peer::start(Some(&root));
    peer.send_raw(br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"nonexistent","reason":"done"}}"#);
    assert_eq!(
        peer.ask(modern(json!(1), "server/discover", json!({})))["result"]["resultType"],
        "complete"
    );
    assert!(peer.finish().0.success());
}

#[test]
fn modern_legacy_client_info_title() {
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    let mut modern_peer = Peer::start(Some(&root));
    let mut modern_bad = fixture()["modernMeta"].clone();
    modern_bad["io.modelcontextprotocol/clientInfo"] = json!({"name":"x","version":"1","title":42});
    error(
        &modern_peer.ask(request(
            json!(7),
            "server/discover",
            json!({"_meta":modern_bad.clone()}),
        )),
        -32602,
        json!(7),
    );
    modern_bad["io.modelcontextprotocol/clientInfo"] =
        json!({"name":"x","version":"1","title":"Valid","futureField":42});
    assert_eq!(
        modern_peer.ask(request(
            json!(8),
            "server/discover",
            json!({"_meta":modern_bad})
        ))["result"]["resultType"],
        "complete"
    );
    error(
        &modern_peer.ask(request(
            json!(9),
            "initialize",
            fixture()["legacyInitialize"].clone(),
        )),
        -32600,
        json!(9),
    );
    assert!(modern_peer.finish().0.success());
    let mut legacy_peer = Peer::start(Some(&root));
    let mut legacy_bad = fixture()["legacyInitialize"].clone();
    legacy_bad["clientInfo"]["title"] = json!(42);
    error(
        &legacy_peer.ask(request(json!(7), "initialize", legacy_bad.clone())),
        -32602,
        json!(7),
    );
    legacy_bad["clientInfo"]["title"] = json!("Valid");
    legacy_bad["clientInfo"]["futureField"] = json!(true);
    assert_eq!(
        legacy_peer.ask(request(json!(8), "initialize", legacy_bad))["result"]["protocolVersion"],
        "2025-11-25"
    );
    legacy_peer.send_raw(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    error(
        &legacy_peer.ask(modern(json!(9), "server/discover", json!({}))),
        -32600,
        json!(9),
    );
    assert!(legacy_peer.finish().0.success());
    for bad_modern in [true, false] {
        let mut peer = Peer::start(Some(&root));
        if bad_modern {
            let mut meta = fixture()["modernMeta"].clone();
            meta["io.modelcontextprotocol/clientInfo"] =
                json!({"name":"x","version":"1","title":42});
            error(
                &peer.ask(request(json!(1), "server/discover", json!({"_meta":meta}))),
                -32602,
                json!(1),
            );
            assert_eq!(
                peer.ask(request(
                    json!(2),
                    "initialize",
                    fixture()["legacyInitialize"].clone()
                ))["result"]["protocolVersion"],
                "2025-11-25"
            );
        } else {
            let mut args = fixture()["legacyInitialize"].clone();
            args["clientInfo"]["title"] = json!(42);
            error(
                &peer.ask(request(json!(1), "initialize", args)),
                -32602,
                json!(1),
            );
            assert_eq!(
                peer.ask(modern(json!(2), "server/discover", json!({})))["result"]["resultType"],
                "complete"
            );
        }
        assert!(peer.finish().0.success());
    }
    let implicit = Peer::start(None);
    let diagnostic = silent_failure(implicit);
    assert!(
        matches!(
            diagnostic.trim(),
            "Error: workspace root overlaps fixed topology"
                | "Error: implicit home or filesystem root refused"
        ),
        "{diagnostic}"
    );
}

fn alternate_token(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\u0022"),
            '\\' => out.push_str("\\u005c"),
            'a' => out.push('a'),
            c if (c as u32) <= 0xffff && ((c as u32) < 32 || !c.is_ascii()) => {
                out.push_str(&format!("\\u{:04x}", c as u32))
            }
            c if (c as u32) > 0xffff => {
                let scalar = (c as u32) - 0x10000;
                out.push_str(&format!(
                    "\\u{:04x}\\u{:04x}",
                    0xd800 + (scalar >> 10),
                    0xdc00 + (scalar & 0x3ff)
                ));
            }
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}
fn boundary_request(id_token: &str, legacy: bool) -> Vec<u8> {
    let (method, params) = if legacy {
        ("initialize", fixture()["legacyInitialize"].clone())
    } else {
        ("server/discover", json!({"_meta":fixture()["modernMeta"]}))
    };
    let bytes =
        format!(r#"{{"jsonrpc":"2.0","id":{id_token},"method":"{method}","params":{params}}}"#)
            .into_bytes();
    assert!(bytes.len() < 16_384, "input line must stay bounded");
    bytes
}
#[test]
fn id_boundaries_binary_both_modes_and_unselected() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let fixture = fixture();
    for row in fixture["idBoundaries"].as_array().unwrap() {
        let unit = row["unit"].as_str().unwrap();
        let repeat = row["repeats"].as_u64().unwrap() as usize;
        let tail = row["tail"].as_u64().unwrap() as usize;
        let cost = row["cost"].as_u64().unwrap() as usize;
        let admitted = format!("{}{}", unit.repeat(repeat), "a".repeat(tail));
        let rejected = format!("{admitted}a");
        assert_eq!(2 + repeat * cost + tail, 256, "{}", row["label"]);
        assert_eq!(2 + repeat * cost + tail + 1, 257, "{}", row["label"]);
        for legacy in [false, true] {
            let mut peer = Peer::start(Some(&root));
            // Both spellings must have exactly the same decoded admission and rejection.
            for invalid in [
                serde_json::to_string(&rejected).unwrap(),
                alternate_token(&rejected),
            ] {
                peer.send_raw(&boundary_request(&invalid, legacy));
                error(&peer.reply(), -32600, Value::Null);
            }
            let valid = alternate_token(&admitted);
            peer.send_raw(&boundary_request(&valid, legacy));
            let first = peer.reply();
            assert_eq!(first["id"], admitted, "{}", row["label"]);
            if legacy {
                peer.send_raw(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
            }
            for (token, expected) in [
                (
                    serde_json::to_string(&admitted).unwrap(),
                    Some(admitted.as_str()),
                ),
                (alternate_token(&admitted), Some(admitted.as_str())),
                (serde_json::to_string(&rejected).unwrap(), None),
                (alternate_token(&rejected), None),
            ] {
                let method = if legacy {
                    "tools/list"
                } else {
                    "server/discover"
                };
                let params = if legacy {
                    json!({})
                } else {
                    json!({"_meta":fixture["modernMeta"]})
                };
                let input = format!(
                    r#"{{"jsonrpc":"2.0","id":{token},"method":"{method}","params":{params}}}"#
                );
                assert!(input.len() < 16_384);
                peer.send_raw(input.as_bytes());
                let reply = peer.reply();
                if let Some(id) = expected {
                    assert_eq!(reply["id"], id, "{}", row["label"]);
                    assert!(reply["result"].is_object());
                } else {
                    error(&reply, -32600, Value::Null);
                    assert!(!reply.to_string().contains(&rejected));
                }
            }
            assert!(peer.finish().0.success());
        }
    }
    for (good, bad) in [
        ("\"".repeat(127), "\"".repeat(128)),
        ("😀".repeat(63), "😀".repeat(64)),
    ] {
        for legacy in [false, true] {
            let mut peer = Peer::start(Some(&root));
            peer.send_raw(&boundary_request(
                &serde_json::to_string(&bad).unwrap(),
                legacy,
            ));
            error(&peer.reply(), -32600, Value::Null);
            peer.send_raw(&boundary_request(&alternate_token(&good), legacy));
            assert_eq!(peer.reply()["id"], good);
            assert!(peer.finish().0.success());
        }
    }
    for legacy in [false, true] {
        let mut peer = Peer::start(Some(&root));
        if legacy {
            ready_legacy(&mut peer);
        }
        for (spelling, integer) in [
            ("1", 1),
            ("1e0", 1),
            ("1.0", 1),
            ("-0", 0),
            ("-9007199254740991", -9007199254740991_i64),
        ] {
            let params = if legacy {
                json!({})
            } else {
                json!({"_meta":fixture["modernMeta"]})
            };
            let method = if legacy {
                "tools/list"
            } else {
                "server/discover"
            };
            let line = format!(
                r#"{{"jsonrpc":"2.0","id":{spelling},"method":"{method}","params":{params}}}"#
            );
            peer.send_raw(line.as_bytes());
            assert_eq!(peer.reply()["id"], integer);
        }
        assert!(peer.finish().0.success());
    }
    // Distinct scalar spelling, decomposed Unicode, escaped literal and unescaped slash/U+2028.
    let mut peer = Peer::start(Some(&root));
    for (token, decoded) in [
        (r#""\u0061""#, "a"),
        (r#""a""#, "a"),
        (r#""é""#, "é"),
        (r#""\u00e9""#, "é"),
        (r#""e\u0301""#, "e\u{0301}"),
        (r#""/ ""#, "/ "),
        (r#""\/\u2028""#, "/ "),
    ] {
        peer.send_raw(&boundary_request(token, false));
        assert_eq!(peer.reply()["id"], decoded);
    }
    for bad in [r#""\ud83d""#, r#""\ude00""#, r#""\ud83d\u0061""#] {
        peer.send_raw(&boundary_request(bad, false));
        error(&peer.reply(), -32700, Value::Null);
    }
    assert!(peer.finish().0.success());
    // Neither rejected first request can select a mode or echo the unsafe ID.
    for initial_legacy in [false, true] {
        let mut peer = Peer::start(Some(&root));
        peer.send_raw(&boundary_request(
            &serde_json::to_string(&"a".repeat(255)).unwrap(),
            initial_legacy,
        ));
        error(&peer.reply(), -32600, Value::Null);
        let response = if initial_legacy {
            peer.ask(modern(json!(2), "server/discover", json!({})))
        } else {
            peer.ask(request(
                json!(2),
                "initialize",
                fixture["legacyInitialize"].clone(),
            ))
        };
        assert!(response["result"].is_object());
        assert!(peer.finish().0.success());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn golden_tool_envelopes_and_maximum_ids_both_modes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    fs::write(root.join("src-sentinel.txt"), b"original source bytes").unwrap();
    let heavy_id = format!(
        "{}{}{}{}",
        "\"".repeat(80),
        "\\".repeat(20),
        "\u{0000}".repeat(7),
        "a".repeat(12)
    );
    assert_eq!(2 + 80 * 2 + 20 * 2 + 7 * 6 + 12, 256);
    let names = fixture()["names"].as_array().unwrap().clone();
    let root_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .root_key;
    for legacy in [false, true] {
        let (mut peer, witness) = Peer::start_with_causal_witness(Some(&root));
        let mut settled_pin: Option<Value> = None;
        if legacy {
            ready_legacy(&mut peer)
        } else {
            assert_eq!(
                peer.ask(modern(json!(1), "server/discover", json!({})))["result"]["resultType"],
                "complete"
            );
        }
        no_db_under(peer._home.path());
        for id in [
            json!(heavy_id),
            json!("😀".repeat(63) + "aa"),
            json!(9007199254740991_i64),
            json!(-9007199254740991_i64),
        ] {
            for (index, name) in names.iter().enumerate() {
                let name = name.as_str().unwrap();
                let bad = json!({"schemaVersion":1,"unknown":"rejected"});
                let invalid = peer.ask(call(legacy, id.clone(), name, bad));
                exact_tool_for_id(&invalid, &id, Some("invalid_request"), !legacy, "checkout");
                let args = fixture()["validArguments"][index].clone();
                let accepted = peer.ask(call(legacy, id.clone(), name, args));
                if index == 0 {
                    exact_tool_for_id(&accepted, &id, None, !legacy, "checkout");
                    if settled_pin.is_none() {
                        settled_pin = Some(await_settled_golden_h(&witness, &root_key).await);
                    }
                } else {
                    let published = settled_pin.as_ref().expect("same-lineage H and watch ACK");
                    let workspace = &accepted["result"]["structuredContent"]["workspace"];
                    let expected = match index {
                        1 => golden_settled_empty_find(&id, workspace, published),
                        2 => golden_settled_failure(&id, workspace, "not_found", published.clone()),
                        3 => golden_settled_failure(&id, workspace, "not_found", published.clone()),
                        _ => unreachable!("fixed golden tool order"),
                    };
                    exact_golden_settled(&accepted, &id, !legacy, expected);
                }
            }
        }
        for name in &names {
            let name = name.as_str().unwrap();
            for args in [json!([]), json!(42), Value::Null] {
                exact_tool_for_id(
                    &peer.ask(call(legacy, json!(heavy_id), name, args)),
                    &json!(heavy_id),
                    Some("invalid_request"),
                    !legacy,
                    "checkout",
                );
            }
        }
        let pin = json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":9007199254740991_u64});
        for (name, args) in [
            (
                "trellis_find_symbols",
                json!({"schemaVersion":1,"query":"x","expectedBasis":pin}),
            ),
            (
                "trellis_inspect",
                json!({"schemaVersion":1,"symbolId":format!("sid:v1:{}","a".repeat(32)),"view":"outgoing_calls","limit":50,"expectedBasis":pin}),
            ),
            (
                "trellis_read_source",
                json!({"schemaVersion":1,"path":"src-sentinel.txt","startLine":1,"endLine":200,"expectedBasis":pin,"expectedContentHash":"a".repeat(64)}),
            ),
        ] {
            let answer = peer.ask(call(legacy, json!(heavy_id), name, args));
            let published = settled_pin.as_ref().expect("same-lineage H and watch ACK");
            let workspace = &answer["result"]["structuredContent"]["workspace"];
            let (code, basis) = ("revision_conflict", published.clone());
            exact_golden_settled(
                &answer,
                &json!(heavy_id),
                !legacy,
                golden_settled_failure(&json!(heavy_id), workspace, code, basis),
            );
        }
        let mut wrong = fixture()["validArguments"][1].clone();
        wrong["limit"] = json!(51);
        exact_tool_for_id(
            &peer.ask(call(legacy, json!(heavy_id), "trellis_find_symbols", wrong)),
            &json!(heavy_id),
            Some("range_too_large"),
            !legacy,
            "checkout",
        );
        for fields in [
            json!({"name":"trellis_workspace_describe","arguments":{},"unexpected":0}),
            json!({"name":"not_a_tool","arguments":{}}),
            json!({"arguments":{}}),
        ] {
            let response = peer.ask(if legacy {
                request(json!(heavy_id), "tools/call", fields)
            } else {
                modern(json!(heavy_id), "tools/call", fields)
            });
            error(&response, -32602, json!(heavy_id));
        }
        assert!(peer.finish().0.success());
    }
    assert_eq!(
        fs::read(root.join("src-sentinel.txt")).unwrap(),
        b"original source bytes"
    );
    fs::write(root.join("src-sentinel.txt"), b"changed source contents").unwrap();
    let mut peer = Peer::start(Some(&root));
    let args = json!({"schemaVersion":1,"path":"src-sentinel.txt","startLine":1,"endLine":1,"expectedContentHash":"a".repeat(64)});
    exact_tool_for_id(
        &peer.ask(call(false, json!(heavy_id), "trellis_read_source", args)),
        &json!(heavy_id),
        Some("index_not_ready"),
        true,
        "checkout",
    );
    assert!(peer.finish().0.success());
}

fn bounded_output(mut command: Command) -> std::process::Output {
    let capture = tempfile::tempdir().unwrap();
    let stdout_path = capture.path().join("stdout.log");
    let stderr_path = capture.path().join("stderr.log");
    let stdout = fs::File::create(&stdout_path).unwrap();
    let stderr = fs::File::create(&stderr_path).unwrap();
    let mut child = command
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .unwrap();
    let (status, expired) = bounded_group_wait(&mut child);
    assert!(!expired, "subprocess exceeded 10-second deadline");
    std::process::Output {
        status,
        stdout: capture_file(&stdout_path),
        stderr: capture_file(&stderr_path),
    }
}
fn silent_failure(mut peer: Peer) -> String {
    peer.stdin.take();
    let (_tx, substitute) = mpsc::channel();
    let lines = std::mem::replace(&mut peer.lines, substitute);
    let (status, stderr) = peer.finish();
    assert!(!status.success(), "expected rejected workspace");
    assert!(
        status.signal().is_none(),
        "startup refusal must exit naturally, not from a signal"
    );
    assert!(!stderr.is_empty(), "diagnostics belong on stderr");
    assert!(
        drain_stdout(lines).is_empty(),
        "startup failure emitted stdout"
    );
    stderr
}
#[test]
fn workspace_selection_startup_side_effects_and_final_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let ancestor = tmp.path().join("ancestor");
    fs::create_dir_all(ancestor.join(".git")).unwrap();
    let nearer = ancestor.join("nearer");
    fs::create_dir_all(nearer.join(".git")).unwrap();
    let nested = nearer.join("deep");
    fs::create_dir(&nested).unwrap();
    // Explicit root wins even if cwd has a nearer Git marker.
    let mut explicit = Peer::start_in(Some(&ancestor), Some(&nested));
    no_db_under(explicit._home.path());
    exact_tool(
        &explicit.ask(modern(
            json!(1),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}),
        )),
        None,
        true,
        "ancestor",
    );
    let ancestor_marker = ancestor.join(".git/trellis/workspace-id");
    let first_marker = fs::read(&ancestor_marker).unwrap();
    assert!(!nearer.join(".git/trellis/workspace-id").exists());
    assert!(explicit.finish().0.success());
    // Adopt the same marker on restart; never regenerate a valid existing UUID.
    let mut adopted = Peer::start(Some(&ancestor));
    exact_tool(
        &adopted.ask(modern(
            json!(2),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}),
        )),
        None,
        true,
        "ancestor",
    );
    assert!(adopted.finish().0.success());
    assert_eq!(fs::read(&ancestor_marker).unwrap(), first_marker);
    // Implicit selection walks to the closest Git directory, not a farther ancestor.
    let mut implicit = Peer::start_in(None, Some(&nested));
    exact_tool(
        &implicit.ask(modern(
            json!(3),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}),
        )),
        None,
        true,
        "nearer",
    );
    let nearer_marker = fs::read(nearer.join(".git/trellis/workspace-id")).unwrap();
    assert_ne!(first_marker, nearer_marker);
    assert!(implicit.finish().0.success());
    // A non-Git cwd cannot acquire a daemon checkout attachment or marker.
    let plain = tmp.path().join("plain");
    fs::create_dir(&plain).unwrap();
    silent_failure(Peer::start_in(None, Some(&plain)));
    assert!(!plain.join(".git").exists());
    // A nearer malformed .git is not silently skipped for ancestor fallback.
    let bad = ancestor.join("bad-nearer");
    fs::create_dir(&bad).unwrap();
    fs::write(bad.join(".git"), "not a valid gitdir pointer").unwrap();
    silent_failure(Peer::start_in(None, Some(&bad)));
    // Final symlinks are refused rather than canonicalized into a different root.
    let linked = tmp.path().join("symlink-root");
    std::os::unix::fs::symlink(&ancestor, &linked).unwrap();
    silent_failure(Peer::start(Some(&linked)));
    silent_failure(Peer::start_in(None, Some(Path::new("/"))));
    // The selected root identity is rechecked for every valid known-tool outcome.
    let mut marker_lost = Peer::start(Some(&ancestor));
    assert!(marker_lost.ask(modern(json!(40), "server/discover", json!({})))["result"].is_object());
    fs::remove_file(&ancestor_marker).unwrap();
    exact_selection_failure(
        &marker_lost.ask(call(
            false,
            json!(5),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        )),
        5,
        "identity_changed",
        &ancestor.canonicalize().unwrap(),
    );
    exact_selection_failure(
        &marker_lost.ask(call(
            false,
            json!(6),
            "trellis_workspace_describe",
            json!({"schemaVersion":1,"workspace":"escape"}),
        )),
        6,
        "identity_changed",
        Path::new("escape"),
    );
    no_db_under(marker_lost._home.path());
    assert!(marker_lost.finish().0.success());
    let mut root_changed = Peer::start(Some(&nearer));
    assert!(
        root_changed.ask(modern(json!(41), "server/discover", json!({})))["result"].is_object()
    );
    let moved = ancestor.join("moved-nearer");
    fs::rename(&nearer, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &nearer).unwrap();
    exact_selection_failure(
        &root_changed.ask(call(
            false,
            json!(7),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"a"}),
        )),
        7,
        "identity_changed",
        &ancestor.canonicalize().unwrap().join("nearer"),
    );
    assert!(root_changed.finish().0.success());
}

#[test]
fn exact_frames_split_utf8_crlf_and_eof() {
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    let mut peer = Peer::start(Some(&root));
    let mut req = modern(json!(50), "server/discover", json!({}));
    req["params"]["_meta"]["padding"] = json!("");
    let empty = serde_json::to_vec(&req).unwrap();
    let padding = "a".repeat(16_384 - empty.len());
    req["params"]["_meta"]["padding"] = json!(padding);
    let raw = serde_json::to_vec(&req).unwrap();
    assert_eq!(raw.len(), 16_384);
    peer.send_raw(&raw);
    assert_eq!(peer.reply()["id"], 50);
    let mut oversized = raw.clone();
    oversized.push(b'\r');
    peer.send_raw(&oversized);
    error(&peer.reply(), -32600, Value::Null);
    // Exactly 16,383 JSON bytes plus CR are 16,384 bytes before LF: admitted.
    req["params"]["_meta"]["padding"] = json!("a".repeat(16_383 - empty.len()));
    let mut exact_crlf = serde_json::to_vec(&req).unwrap();
    assert_eq!(exact_crlf.len(), 16_383);
    exact_crlf.push(b'\r');
    peer.send_raw(&exact_crlf);
    assert_eq!(peer.reply()["id"], 50);
    let mut invalid_utf8 = br#"{"jsonrpc":"2.0","id":"x","method":"server/discover"}"#.to_vec();
    let marker = invalid_utf8.windows(3).position(|p| p == b"\"x\"").unwrap();
    invalid_utf8[marker + 1] = 0xff;
    peer.send_raw(&invalid_utf8);
    error(&peer.reply(), -32700, Value::Null);
    assert_eq!(
        peer.ask(modern(json!(53), "server/discover", json!({})))["id"],
        53,
        "bad UTF-8 must not poison the next frame"
    );
    let mut crlf = serde_json::to_vec(&modern(json!(51), "server/discover", json!({}))).unwrap();
    crlf.push(b'\r');
    peer.send_raw(&crlf);
    assert_eq!(peer.reply()["id"], 51);
    let mut unicode =
        serde_json::to_vec(&modern(json!("é😀"), "server/discover", json!({}))).unwrap();
    let at = unicode
        .windows(4)
        .position(|bytes| bytes == "😀".as_bytes())
        .unwrap();
    let tail = unicode.split_off(at + 1);
    peer.stdin.as_mut().unwrap().write_all(&unicode).unwrap();
    peer.stdin.as_mut().unwrap().flush().unwrap();
    peer.send_raw(&tail);
    assert_eq!(peer.reply()["id"], "é😀");
    assert!(peer.finish().0.success());
    let mut incomplete = Peer::start(Some(&root));
    incomplete
        .stdin
        .as_mut()
        .unwrap()
        .write_all(
            fixture()["transcripts"]["incompleteEof"]["input"]
                .as_str()
                .unwrap()
                .as_bytes(),
        )
        .unwrap();
    let (_tx, substitute) = mpsc::channel();
    let lines = std::mem::replace(&mut incomplete.lines, substitute);
    assert!(incomplete.finish().0.success());
    assert!(
        drain_stdout(lines).is_empty(),
        "unterminated input cannot be dispatched"
    );
    // A completed request remains silent after its cancellation and at EOF.
    let mut completed = Peer::start(Some(&root));
    assert_eq!(
        completed.ask(modern(json!(52), "server/discover", json!({})))["id"],
        52
    );
    let mut cancelled = fixture()["transcripts"]["completedCancellation"]["notification"].clone();
    cancelled["params"]["requestId"] = json!(52);
    completed.send_raw(serde_json::to_string(&cancelled).unwrap().as_bytes());
    let (_tx, substitute) = mpsc::channel();
    let lines = std::mem::replace(&mut completed.lines, substitute);
    assert!(completed.finish().0.success());
    assert!(
        drain_stdout(lines).is_empty(),
        "cancelled completed request/EOF emitted a response"
    );
}

#[test]
fn unusable_index_sentinels_do_not_change_unavailable_projection_or_get_repaired() {
    use sha2::{Digest, Sha256};
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    let root_key = hex::encode(Sha256::digest(
        fs::canonicalize(&root)
            .unwrap()
            .to_str()
            .unwrap()
            .as_bytes(),
    ));
    let mut peer = Peer::start(Some(&root));
    assert_eq!(
        peer.ask(modern(json!(1), "server/discover", json!({})))["result"]["resultType"],
        "complete"
    );
    let home = peer._home.path();
    let caches = [
        home.join("Library/Caches/dev.squashmerge.trellis"),
        home.join("cache/trellis"),
        home.join("cache/dev/squashmerge/trellis"),
    ];
    let mut sentinels = Vec::new();
    for cache in caches {
        let index = cache.join("indexes").join(&root_key).join("index.db");
        fs::create_dir_all(index.parent().unwrap()).unwrap();
        fs::write(&index, b"unusable-index-DO-NOT-OPEN").unwrap();
        sentinels.push(index);
    }
    for name in [
        "trellis_find_symbols",
        "trellis_inspect",
        "trellis_read_source",
    ] {
        let i = fixture()["names"]
            .as_array()
            .unwrap()
            .iter()
            .position(|v| v == name)
            .unwrap();
        let response = peer.ask(call(
            false,
            json!(2),
            name,
            fixture()["validArguments"][i].clone(),
        ));
        exact_tool(&response, Some("index_not_ready"), true, "checkout");
    }
    no_db_under_except(peer._home.path(), &sentinels);
    for index in sentinels {
        assert_eq!(fs::read(&index).unwrap(), b"unusable-index-DO-NOT-OPEN");
        assert!(!index.with_extension("db-wal").exists());
        assert!(!index.with_extension("db-shm").exists());
    }
    assert!(!root.join(".git/trellis/requests.db").exists());
    assert!(peer.finish().0.success());
}

#[test]
fn golden_modern_legacy_transcripts() {
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    for mode in ["modern", "legacy"] {
        let fixture = fixture();
        let transcript = &fixture["transcripts"][mode];
        let mut peer = Peer::start(Some(&root));
        let mut expected = transcript["response"].clone();
        if mode == "modern" {
            expected["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["version"] =
                json!(env!("CARGO_PKG_VERSION"));
        } else {
            expected["result"]["serverInfo"]["version"] = json!(env!("CARGO_PKG_VERSION"));
        }
        assert_eq!(peer.ask(transcript["request"].clone()), expected);
        if mode == "legacy" {
            peer.send_raw(
                serde_json::to_string(&transcript["initialized"])
                    .unwrap()
                    .as_bytes(),
            );
        }
        let response = peer.ask(call(
            mode == "legacy",
            json!("golden-describe"),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        ));
        exact_tool(&response, None, mode == "modern", "checkout");
        let response = peer.ask(call(
            mode == "legacy",
            json!("golden-evidence"),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"x"}),
        ));
        exact_tool(
            &response,
            Some("index_not_ready"),
            mode == "modern",
            "checkout",
        );
        assert!(peer.finish().0.success());
    }
}

#[test]
fn ordinary_cancellation_and_eof_without_private_race_hook() {
    let tmp = tempfile::tempdir().unwrap();
    let root = checkout(tmp.path());
    for legacy in [false, true] {
        let mut peer = Peer::start(Some(&root));
        if legacy {
            ready_legacy(&mut peer)
        }
        let active = call(
            legacy,
            json!("cancel-me"),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"a"}),
        );
        let cancelled = json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"cancel-me","reason":"client no longer needs response"}});
        let sentinel = if legacy {
            request(json!("after-cancel"), "tools/list", json!({}))
        } else {
            modern(json!("after-cancel"), "server/discover", json!({}))
        };
        // One write gives the reader an ordinary input burst, not a barrier between
        // prepare and commit. The old reply may already be committed before cancellation.
        let burst = format!("{}\n{}\n{}\n", active, cancelled, sentinel);
        peer.stdin
            .as_mut()
            .unwrap()
            .write_all(burst.as_bytes())
            .unwrap();
        peer.stdin.as_mut().unwrap().flush().unwrap();
        let mut reply = peer.reply();
        if reply["id"] == "cancel-me" {
            exact_tool(&reply, Some("index_not_ready"), !legacy, "checkout");
            reply = peer.reply();
        }
        assert_eq!(reply["id"], "after-cancel");
        assert!(reply["result"].is_object());
        let (_tx, substitute) = mpsc::channel();
        let lines = std::mem::replace(&mut peer.lines, substitute);
        assert!(peer.finish().0.success());
        assert!(
            drain_stdout(lines).is_empty(),
            "cancellation/EOF emitted an extra response"
        );
    }
    // EOF during an ordinary request can race a completed response. It must
    // terminate without emitting a partial frame or any response after shutdown.
    let mut peer = Peer::start(Some(&root));
    let active = modern(json!("eof-request"), "tools/list", json!({}));
    peer.send_raw(serde_json::to_string(&active).unwrap().as_bytes());
    let (_tx, substitute) = mpsc::channel();
    let lines = std::mem::replace(&mut peer.lines, substitute);
    assert!(peer.finish().0.success());
    let all = drain_stdout(lines);
    assert!(all.len() <= 1, "EOF cannot cause duplicate outcomes");
    for line in all {
        assert!(line.len() <= 65_536);
        assert_eq!(
            line.last(),
            Some(&b'\n'),
            "EOF response must be a complete line"
        );
        let value: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(value["id"], "eof-request");
    }
}

#[test]
fn linked_git_worktrees_get_distinct_selected_markers() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("main-checkout");
    fn git(cwd: &Path, home: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command
            .current_dir(cwd)
            .args(args)
            .env("HOME", home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", Path::new("/dev/null"))
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
        let result = bounded_output(command);
        assert!(
            result.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
    git(
        tmp.path(),
        tmp.path(),
        &["init", "--quiet", repo.to_str().unwrap()],
    );
    git(
        &repo,
        tmp.path(),
        &["commit", "--allow-empty", "--quiet", "-m", "fixture root"],
    );
    let one = tmp.path().join("linked-one");
    let two = tmp.path().join("linked-two");
    for path in [&one, &two] {
        git(
            &repo,
            tmp.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
    }
    fn gitdir(root: &Path) -> PathBuf {
        let pointer = fs::read_to_string(root.join(".git")).unwrap();
        assert!(pointer.starts_with("gitdir: "));
        let relative = pointer.trim_end().strip_prefix("gitdir: ").unwrap();
        fs::canonicalize(root.join(relative)).unwrap()
    }
    let git_one = gitdir(&one);
    let git_two = gitdir(&two);
    assert_ne!(
        git_one, git_two,
        "different linked worktrees need distinct Git dirs"
    );
    let mut ids = Vec::new();
    for (root, label, git) in [
        (&one, "linked-one", &git_one),
        (&two, "linked-two", &git_two),
    ] {
        let mut peer = Peer::start(Some(root));
        no_db_under(peer._home.path());
        let result = peer.ask(call(
            false,
            json!(1),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        ));
        exact_tool_for_id(&result, &json!(1), None, true, label);
        let marker = git.join("trellis/workspace-id");
        ids.push(fs::read(&marker).unwrap());
        assert!(
            !root.join(".git/trellis/workspace-id").exists(),
            "pointer file must not be treated as directory"
        );
        assert!(peer.finish().0.success());
        let mut reopened = Peer::start_in(None, Some(root));
        let result = reopened.ask(call(
            false,
            json!(2),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        ));
        exact_tool_for_id(&result, &json!(2), None, true, label);
        assert!(reopened.finish().0.success());
        assert_eq!(
            fs::read(&marker).unwrap(),
            *ids.last().unwrap(),
            "restart must adopt selected pointer marker"
        );
    }
    assert_ne!(
        ids[0], ids[1],
        "two real linked worktrees must not share a workspace UUID marker"
    );
}

#[test]
fn per_call_linked_worktree_selection_and_closed_failures() {
    assert_eq!(
        fixture()["workspaceSelection"],
        json!({
        "inputField":"workspace","maxUtf8Bytes":4096,"attemptedValueBytes":256,
        "resolvedFields":["workspace","catchingUp"],
        "reasons":["not_absolute","not_checkout","different_repository","identity_changed","unavailable"]})
    );
    fn git(cwd: &Path, args: &[&str]) {
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
    }
    let temp = tempfile::tempdir().unwrap();
    let main = temp.path().join("main");
    git(temp.path(), &["init", "--quiet", main.to_str().unwrap()]);
    git(
        &main,
        &["commit", "--allow-empty", "--quiet", "-m", "fixture"],
    );
    let linked = temp.path().join("linked");
    git(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    let foreign = temp.path().join("foreign");
    git(temp.path(), &["init", "--quiet", foreign.to_str().unwrap()]);
    let main = main.canonicalize().unwrap();
    let linked = linked.canonicalize().unwrap();
    let foreign = foreign.canonicalize().unwrap();
    let foreign_marker = foreign.join(".git/trellis/workspace-id");
    let git_pointer = fs::read_to_string(linked.join(".git")).unwrap();
    let linked_git =
        fs::canonicalize(linked.join(git_pointer.trim().strip_prefix("gitdir: ").unwrap()))
            .unwrap();
    let linked_marker = linked_git.join("trellis/workspace-id");
    for legacy in [false, true] {
        let mut peer = Peer::start(Some(&main));
        if legacy {
            ready_legacy(&mut peer);
        }
        let describe = |peer: &mut Peer, id: i64, args: Value| {
            peer.ask(call(legacy, json!(id), "trellis_workspace_describe", args))
        };
        let selected = describe(&mut peer, 10, json!({"schemaVersion":1,"workspace":linked}));
        tool(&selected, None, !legacy);
        assert_eq!(
            selected["result"]["structuredContent"]["workspace"],
            json!(linked)
        );
        assert_eq!(selected["result"]["structuredContent"]["catchingUp"], true);
        assert_eq!(
            selected["result"]["structuredContent"]["data"]["workspaceLabel"],
            "linked"
        );
        assert!(linked_marker.exists());
        let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let status = describe(&mut peer, 20, json!({"schemaVersion":1,"workspace":linked}));
            tool(&status, None, !legacy);
            if status["result"]["structuredContent"]["data"]["indexState"] == "ready" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "linked checkout head was not admitted"
            );
            std::thread::yield_now();
        }
        for (offset, name) in fixture()["names"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .skip(1)
        {
            let mut arguments = fixture()["validArguments"][offset].clone();
            arguments["workspace"] = json!(linked);
            let answer = peer.ask(call(
                legacy,
                json!(30 + offset),
                name.as_str().unwrap(),
                arguments,
            ));
            if offset == 1 {
                tool(&answer, None, !legacy);
                let data = &answer["result"]["structuredContent"]["data"];
                assert_eq!(*data, json!({"symbols":[],"coverage":[],"provenance":[]}));
            } else {
                tool(&answer, Some("not_found"), !legacy);
                assert!(answer["result"]["structuredContent"]["error"]["currentBasis"].is_object());
            }
            assert_eq!(
                answer["result"]["structuredContent"]["workspace"],
                json!(linked)
            );
            assert!(answer["result"]["structuredContent"]["catchingUp"].is_boolean());
        }
        let launch = describe(&mut peer, 11, json!({"schemaVersion":1}));
        tool(&launch, None, !legacy);
        assert_eq!(
            launch["result"]["structuredContent"]["workspace"],
            json!(main)
        );
        assert_eq!(
            launch["result"]["structuredContent"]["data"]["workspaceLabel"],
            "main"
        );
        for (id, value, reason) in [
            (12, json!("relative"), "not_absolute"),
            (13, json!(foreign), "different_repository"),
        ] {
            let answer = describe(&mut peer, id, json!({"schemaVersion":1,"workspace":value}));
            tool(&answer, Some("workspace_selection_failed"), !legacy);
            let envelope = &answer["result"]["structuredContent"];
            assert!(envelope.get("workspace").is_none());
            assert!(envelope.get("catchingUp").is_none());
            assert_eq!(envelope["error"]["reason"], reason);
            assert_eq!(envelope["error"]["attemptedWorkspace"]["value"], value);
            assert_eq!(envelope["error"]["currentBasis"], Value::Null);
        }
        assert!(
            !foreign_marker.exists(),
            "foreign selection cannot create a marker"
        );
        let symlink = temp.path().join("link-to-linked");
        if !symlink.exists() {
            std::os::unix::fs::symlink(&linked, &symlink).unwrap();
        }
        let answer = describe(
            &mut peer,
            14,
            json!({"schemaVersion":1,"workspace":symlink}),
        );
        tool(&answer, Some("workspace_selection_failed"), !legacy);
        assert_eq!(
            answer["result"]["structuredContent"]["error"]["reason"],
            "not_checkout"
        );
        for (offset, (value, json_type)) in [
            (Value::Null, "null"),
            (json!(true), "boolean"),
            (json!(42), "number"),
            (json!([]), "array"),
            (json!({"unexpected":"content"}), "object"),
        ]
        .into_iter()
        .enumerate()
        {
            let invalid = describe(
                &mut peer,
                150 + offset as i64,
                json!({"schemaVersion":1,"workspace":value}),
            );
            tool(&invalid, Some("invalid_request"), !legacy);
            assert!(
                invalid.get("error").is_none(),
                "not JSON-RPC invalid_params"
            );
            let result = &invalid["result"]["structuredContent"];
            assert_eq!(
                result["error"]["attemptedWorkspace"],
                json!({"kind":"invalid_type","jsonType":json_type}),
            );
            assert_eq!(result["error"]["currentBasis"], Value::Null);
            assert_eq!(result["error"]["currentContentHash"], Value::Null);
            assert!(result.get("workspace").is_none());
            assert!(result.get("catchingUp").is_none());
        }
        let long = format!("{}😀", "é".repeat(2047));
        let invalid = describe(&mut peer, 16, json!({"schemaVersion":1,"workspace":long}));
        tool(&invalid, Some("invalid_request"), !legacy);
        let attempt = &invalid["result"]["structuredContent"]["error"]["attemptedWorkspace"];
        assert_eq!(attempt["utf8Bytes"], 4098);
        assert_eq!(attempt["value"].as_str().unwrap().len(), 256);
        assert_eq!(attempt["truncated"], true);
        assert_eq!(
            describe(&mut peer, 17, json!({"schemaVersion":1}))["result"]["structuredContent"]["workspace"],
            json!(main)
        );
        assert!(peer.finish().0.success());
    }
}

#[test]
fn catalog_rejects_contradictory_unresolved_and_resolved_failures() {
    let catalog: Value = serde_json::from_str(include_str!("fixtures/mcp/catalog.json")).unwrap();
    assert_eq!(catalog.as_array().unwrap().len(), 4);
    for entry in catalog.as_array().unwrap() {
        let defs = &entry["outputSchema"]["$defs"];
        assert_eq!(
            defs["Failure"]["properties"]["error"]["properties"]["code"],
            json!({"$ref":"#/$defs/ResolvedFailureCode"})
        );
        assert!(
            !defs["ResolvedFailureCode"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("workspace_selection_failed"))
        );
        for kind in ["SelectionFailure", "InvalidWorkspaceFailure"] {
            let error = &defs[kind]["properties"]["error"]["properties"];
            assert_eq!(error["currentBasis"], json!({"const":null}));
            assert_eq!(error["currentContentHash"], json!({"const":null}));
        }
        assert_eq!(
            defs["InvalidWorkspaceFailure"]["properties"]["error"]["properties"]["retryable"],
            json!({"const":false})
        );
        assert_eq!(
            defs["SelectionFailure"]["properties"]["error"]["allOf"][0]["then"]["properties"]["retryable"],
            json!({"const":true})
        );
        assert_eq!(
            defs["SelectionFailure"]["properties"]["error"]["allOf"][0]["else"]["properties"]["retryable"],
            json!({"const":false})
        );
    }
}

fn git_selection(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(dir)
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
}
fn git_linked_fixture(temp: &Path) -> (PathBuf, PathBuf) {
    let main = temp.join("main");
    git_selection(temp, &["init", "--quiet", main.to_str().unwrap()]);
    git_selection(
        &main,
        &["commit", "--allow-empty", "--quiet", "-m", "fixture"],
    );
    let linked = temp.join("linked");
    git_selection(
        &main,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    (main.canonicalize().unwrap(), linked.canonicalize().unwrap())
}
fn linked_marker(linked: &Path) -> PathBuf {
    let pointer = fs::read_to_string(linked.join(".git")).unwrap();
    fs::canonicalize(linked.join(pointer.trim().strip_prefix("gitdir: ").unwrap()))
        .unwrap()
        .join("trellis/workspace-id")
}

#[test]
fn selected_clone_failure_is_resolved_and_does_not_kill_the_session() {
    for legacy in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (main, linked) = git_linked_fixture(temp.path());
        let socket = temp.path().join("phase.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut peer = Peer::start_with_phase(Some(&main), None, Some(("selected_clone", &socket)));
        if legacy {
            ready_legacy(&mut peer);
        }
        peer.send_raw(
            serde_json::to_string(&call(
                legacy,
                json!(8),
                "trellis_workspace_describe",
                json!({"schemaVersion":1,"workspace":linked}),
            ))
            .unwrap()
            .as_bytes(),
        );
        let marker = linked_marker(&linked);
        phase_barrier(&listener, "selected_clone", || {
            fs::remove_file(&marker).unwrap()
        });
        let reply = peer.reply();
        tool(&reply, Some("root_changed"), !legacy);
        assert_eq!(
            reply["result"]["structuredContent"]["workspace"],
            json!(linked)
        );
        assert_eq!(reply["result"]["structuredContent"]["catchingUp"], true);
        assert_eq!(
            reply["result"]["structuredContent"]["error"]["currentBasis"],
            Value::Null
        );
        assert!(
            peer.finish().0.success(),
            "daemon panic cannot interrupt thin client"
        );
    }
}

#[test]
fn final_resolved_drift_is_attributed_for_four_tools_and_both_modes() {
    for legacy in [false, true] {
        for (index, name) in fixture()["names"].as_array().unwrap().iter().enumerate() {
            for explicit in [false, true] {
                let temp = tempfile::tempdir().unwrap();
                let (main, linked) = git_linked_fixture(temp.path());
                let root = if explicit { &linked } else { &main };
                let stage = if explicit {
                    "selected_final"
                } else {
                    "launch_final"
                };
                let socket = temp.path().join("phase.sock");
                let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
                let mut peer = Peer::start_with_phase(Some(&main), None, Some((stage, &socket)));
                if legacy {
                    ready_legacy(&mut peer);
                }
                let mut args = fixture()["validArguments"][index].clone();
                if explicit {
                    args["workspace"] = json!(linked);
                }
                peer.send_raw(
                    serde_json::to_string(&call(
                        legacy,
                        json!(index),
                        name.as_str().unwrap(),
                        args,
                    ))
                    .unwrap()
                    .as_bytes(),
                );
                if index % 2 == 0 {
                    let marker = if explicit {
                        linked_marker(root)
                    } else {
                        root.join(".git/trellis/workspace-id")
                    };
                    let mut saved = Vec::new();
                    phase_barrier(&listener, stage, || {
                        saved = fs::read(&marker).unwrap();
                        fs::remove_file(&marker).unwrap();
                    });
                    let reply = peer.reply();
                    tool(&reply, Some("root_changed"), !legacy);
                    let envelope = &reply["result"]["structuredContent"];
                    assert_eq!(envelope["workspace"], json!(root));
                    assert_eq!(envelope["catchingUp"], true);
                    assert_eq!(envelope["error"]["currentBasis"], Value::Null);
                    assert!(envelope["error"].get("reason").is_none());
                    fs::write(&marker, saved).unwrap();
                } else {
                    let moved = temp.path().join(if explicit {
                        "moved-linked"
                    } else {
                        "moved-main"
                    });
                    phase_barrier(&listener, stage, || fs::rename(root, &moved).unwrap());
                    let reply = peer.reply();
                    tool(&reply, Some("root_changed"), !legacy);
                    assert_eq!(
                        reply["result"]["structuredContent"]["workspace"],
                        json!(root)
                    );
                    assert_eq!(
                        reply["result"]["structuredContent"]["error"]["currentBasis"],
                        Value::Null
                    );
                    fs::rename(moved, root).unwrap();
                }
                assert!(peer.finish().0.success());
            }
        }
    }
}

#[test]
fn capacity_witness_final_drift_refuses_stale_capacity_attribution() {
    let temp = tempfile::tempdir().unwrap();
    let (main, first) = git_linked_fixture(temp.path());
    let mut linked = vec![first];
    for index in 1..=63 {
        let path = temp.path().join(format!("linked-{index}"));
        git_selection(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                path.to_str().unwrap(),
                "HEAD",
            ],
        );
        linked.push(path.canonicalize().unwrap());
    }
    let socket = temp.path().join("phase.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let mut peer = Peer::start_with_phase(Some(&main), None, Some(("capacity_final", &socket)));
    for (index, path) in linked.iter().take(63).enumerate() {
        let reply = peer.ask(call(
            false,
            json!(index),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"capacity","workspace":path}),
        ));
        tool(&reply, Some("index_not_ready"), true);
        assert_eq!(
            reply["result"]["structuredContent"]["workspace"],
            json!(path)
        );
    }
    let denied = &linked[63];
    peer.send_raw(
        serde_json::to_string(&call(
            false,
            json!(64),
            "trellis_workspace_describe",
            json!({"schemaVersion":1,"workspace":denied}),
        ))
        .unwrap()
        .as_bytes(),
    );
    let marker = linked_marker(denied);
    phase_barrier(&listener, "capacity_final", || {
        fs::remove_file(&marker).unwrap()
    });
    let reply = peer.reply();
    tool(&reply, Some("root_changed"), true);
    assert_eq!(
        reply["result"]["structuredContent"]["workspace"],
        json!(denied)
    );
    assert_eq!(reply["result"]["structuredContent"]["catchingUp"], true);
    assert_eq!(
        reply["result"]["structuredContent"]["error"]["currentBasis"],
        Value::Null
    );
    assert!(peer.finish().0.success());
}

/// One bounded, read-only causal event per private fixture socket connection.
async fn next_causal_event(
    listener: &tokio::net::UnixListener,
    deadline: tokio::time::Instant,
) -> Value {
    use tokio::io::AsyncReadExt;
    let (socket, _) = tokio::time::timeout_at(deadline, listener.accept())
        .await
        .expect("daemon did not emit a causal startup/watch witness")
        .unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout_at(deadline, socket.take(513).read_to_end(&mut bytes))
        .await
        .expect("incomplete causal witness")
        .unwrap();
    assert!(
        !bytes.is_empty() && bytes.len() <= 512,
        "invalid causal witness size"
    );
    serde_json::from_slice(&bytes).expect("causal witness must be JSON")
}

/// Generation comparisons are valid only within one owner and watcher epoch.
#[derive(Default)]
struct CausalReadiness {
    stream_id: Option<String>,
    stream_seq: u64,
    lineage_ordinal: Option<u64>,
    lineage: Option<(String, String)>,
    h_ready: bool,
    acked: Option<u64>,
    pending: Option<u64>,
    announced_generations: std::collections::HashSet<u64>,
}

fn causal_lineage(event: &Value) -> (String, String) {
    let field = |name| {
        let value = event[name].as_str().expect("missing causal lineage UUID");
        uuid::Uuid::parse_str(value).expect("invalid causal lineage UUID");
        value.to_owned()
    };
    (field("ownerIncarnation"), field("watchEpoch"))
}

impl CausalReadiness {
    /// Return false for a sequenced but causally retired lineage. Never let an
    /// old epoch's late connection reset a newer owner/watch witness.
    fn observe(&mut self, event: &Value, published: &Value) -> bool {
        let stream_id = event["streamId"].as_str().expect("witness stream ID");
        uuid::Uuid::parse_str(stream_id).expect("invalid witness stream ID");
        if let Some(first) = &self.stream_id {
            assert_eq!(
                stream_id, first,
                "another daemon emitted into this witness stream"
            );
        } else {
            self.stream_id = Some(stream_id.to_owned());
        }
        let sequence = event["streamSeq"]
            .as_u64()
            .expect("witness stream sequence");
        assert_eq!(
            sequence,
            self.stream_seq + 1,
            "lost, duplicate, or reordered causal event"
        );
        self.stream_seq = sequence;
        // Transport failure invalidates the whole stream, even if its last
        // queued record belonged to a now-retired watcher epoch.
        assert_ne!(
            event["kind"], "ERROR",
            "fixture causal witness transport failed: {event}"
        );

        let lineage_ordinal = event["lineageOrdinal"].as_u64().expect("lineage ordinal");
        let lineage = causal_lineage(event);
        match self.lineage_ordinal {
            Some(current) if lineage_ordinal < current => return false,
            Some(current) if lineage_ordinal == current => {
                assert_eq!(
                    self.lineage.as_ref(),
                    Some(&lineage),
                    "lineage ordinal collision"
                );
            }
            _ => {
                self.lineage_ordinal = Some(lineage_ordinal);
                self.lineage = Some(lineage);
                self.h_ready = false;
                self.acked = None;
                self.pending = None;
                self.announced_generations.clear();
            }
        }
        match event["kind"].as_str().expect("witness kind") {
            "H_READY" => {
                assert!(
                    event["pin"].is_object(),
                    "H requires its own publication pin"
                );
                self.h_ready = true;
            }
            "WATCH_PENDING" => {
                let generation = event["watchGeneration"].as_u64().expect("watch generation");
                self.announced_generations.insert(generation);
                self.pending = Some(
                    self.pending
                        .map_or(generation, |prior| prior.max(generation)),
                );
            }
            "ERROR" => panic!("fixture causal witness transport failed: {event}"),
            "WATCH_ACK" => {
                assert_eq!(event["queueEmpty"], true, "ACK requires an empty FIFO");
                let generation = event["watchGeneration"].as_u64().expect("watch generation");
                assert!(
                    self.announced_generations.contains(&generation),
                    "ACK without same-lineage WATCH_PENDING for its generation"
                );
                // CLI row completion is not startup/watch settlement. Earlier startup reconciliation ACKs
                // may be observed, but only the accepted CLI pin binds this ACK.
                if event["pin"] == *published {
                    self.acked = Some(self.acked.map_or(generation, |prior| prior.max(generation)));
                }
            }
            kind => panic!("unknown causal witness: {kind}"),
        }
        true
    }

    fn settled(&self) -> bool {
        self.h_ready
            && self
                .acked
                .is_some_and(|ack| self.pending.is_some_and(|hint| ack >= hint))
    }
}

/// H publication and a same-lineage, empty watch ACK settle this golden
/// checkout. The observed H pin is a causal witness, not a copied tool reply.
async fn await_settled_golden_h(listener: &tokio::net::UnixListener, root_key: &str) -> Value {
    let deadline = tokio::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut events = Vec::new();
    let mut published_pin: Option<Value> = None;
    let mut readiness = CausalReadiness::default();
    for _ in 0..24 {
        let event = next_causal_event(listener, deadline).await;
        assert_eq!(event["rootKey"], root_key, "foreign causal witness");
        if event["kind"] == "H_READY" {
            let pin = &event["pin"];
            uuid::Uuid::parse_str(pin["indexGeneration"].as_str().expect("H generation"))
                .expect("valid H generation UUID");
            assert!(
                pin["indexRevision"]
                    .as_u64()
                    .is_some_and(|revision| revision > 0)
            );
            if let Some(first) = &published_pin {
                assert_eq!(pin, first, "golden pin changed without a fixture edit");
            } else {
                published_pin = Some(pin.clone());
            }
        }
        events.push(event);
        if let Some(pin) = &published_pin {
            for event in events.drain(..) {
                readiness.observe(&event, pin);
            }
            if readiness.settled() {
                return pin.clone();
            }
        }
    }
    panic!("golden H/watch settlement exceeded causal transition budget");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catching_up_reports_pending_work_then_witnessed_readiness_on_same_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let (mut peer, witness) = Peer::start_with_causal_witness(Some(&root));
    let first = peer.ask(call(
        false,
        json!(1),
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
    ));
    tool(&first, None, true);
    // This first true means pending work; without a phase witness it does not
    // distinguish mandatory startup reconciliation from a queued watcher hint.
    assert_eq!(first["result"]["structuredContent"]["catchingUp"], true);
    assert!(first["result"]["structuredContent"]["evidenceBasis"].is_null());
    assert_eq!(
        first["result"]["structuredContent"]["data"]["languages"],
        json!([])
    );
    let root_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .root_key;
    let home = peer._home.path().to_path_buf();
    let mut command = Command::new(env!("CARGO_BIN_EXE_trellis"));
    command
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"));
    let indexed = bounded_output(command);
    assert!(
        indexed.status.success(),
        "index: {}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let cli: Value = serde_json::from_slice(&indexed.stdout).expect("CLI index response");
    let published = &cli["publishedRevision"];
    assert!(published["indexGeneration"].is_string());
    assert!(published["indexRevision"].is_u64());

    // A completed CLI queue row pins a publication, not the daemon's mandatory
    // startup reconciliation or its watcher inventory. The same Peer must keep its launch selection,
    // but a second tool call may still report catchingUp=true.
    let second = peer.ask(call(
        false,
        json!(2),
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
    ));
    tool(&second, None, true);
    let selected = &first["result"]["structuredContent"]["workspace"];
    assert_eq!(
        &second["result"]["structuredContent"]["workspace"],
        selected
    );
    assert!(second["result"]["structuredContent"]["catchingUp"].is_boolean());
    let described = &second["result"]["structuredContent"];
    assert_eq!(described["data"]["indexState"], "ready");
    assert_eq!(described["evidenceBasis"], *published);
    assert_eq!(described["data"]["languages"].as_array().unwrap().len(), 4);
    assert_eq!(described["data"]["languages"][0]["language"], "java");
    assert_eq!(described["data"]["languages"][3]["language"], "javascript");
    assert_eq!(described["partial"], false);

    // Future opt-in daemon hooks send one bounded JSON record per connection:
    // {kind,rootKey,streamId,streamSeq,lineageOrdinal,ownerIncarnation,
    //  watchEpoch,pin,watchGeneration,queueEmpty}. A single FIFO writer sends
    // one complete event per connection. Any missing/duplicate sequence fails.
    // H_READY follows verified owner installation AND phase Ready/startup reconciliation=false.
    // WATCH_ACK follows full inventory acknowledgment with no unfinished FIFO.
    // WATCH_PENDING is
    // emitted for a later accepted hint, so an ACK is never a future quiet lease.
    // The socket is read-only, private to HOME, and does not address a PID.
    let deadline = tokio::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut events = 0;
    let mut readiness = CausalReadiness::default();
    let mut next_event = async || {
        events += 1;
        assert!(events <= 24, "causal witness transition budget exhausted");
        let event = next_causal_event(&witness, deadline).await;
        assert_eq!(event["rootKey"], root_key, "foreign causal witness");
        event
    };
    // startup reconciliation and ACK must be from the SAME verified owner and watch epoch. If a
    // successor takes over, discard the former generation and require new startup reconciliation.
    while !readiness.settled() {
        readiness.observe(&next_event().await, published);
    }

    // One CLI index is the only Q submission in this private fixture. There
    // are no other clients or fixture edits, and 15 seconds is below the 60s
    // periodic inventory interval. If another FIFO row unexpectedly appears,
    // the lack of a watcher witness must fail, not be called a watcher hint.
    // A genuine new hint after ACK can make true legitimate. Retry only after
    // witnessing that generation's ACK; never poll catchingUp or infer false.
    for id in 3..=5 {
        let reply = peer.ask(call(
            false,
            json!(id),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        ));
        tool(&reply, None, true);
        let data = &reply["result"]["structuredContent"];
        assert_eq!(&data["workspace"], selected);
        if data["catchingUp"] == false {
            // Describe has evidenceBasis:null; this proves the selected checkout
            // is now settled, not that ID3 atomically returned the CLI pin.
            assert!(peer.finish().0.success());
            return;
        }
        assert_eq!(data["catchingUp"], true);
        assert!(
            id < 5,
            "new watcher hints exceeded the bounded transition budget"
        );
        let prior_ordinal = readiness
            .lineage_ordinal
            .expect("correlated startup reconciliation/ACK lineage");
        let prior_ack = readiness.acked.expect("CLI-pinned watcher ACK");
        let mut witnessed_later = false;
        loop {
            let event = next_event().await;
            let ordinal = event["lineageOrdinal"].as_u64().expect("lineage ordinal");
            let later_hint = ordinal == prior_ordinal
                && event["kind"] == "WATCH_PENDING"
                && event["watchGeneration"].as_u64().expect("watch generation") > prior_ack;
            let current = readiness.observe(&event, published);
            // Old-epoch deliveries still consume streamSeq but cannot replace
            // a newer startup reconciliation witness or count as a new reason to retry.
            if current && (ordinal > prior_ordinal || later_hint) {
                witnessed_later = true;
            }
            if witnessed_later && readiness.settled() {
                break;
            }
        }
    }
    unreachable!("bounded tool calls return or fail")
}

/// Drive a real same-owner selected-options change through the daemon FIFO.
/// The fresh H_READY is a Ready snapshot for the replacement watcher; it does
/// not claim that mandatory startup reconciliation reran when the accepted CLI index changed inputs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_options_replacement_reissues_ready_for_new_watch_epoch() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let (mut peer, listener) = Peer::start_with_causal_witness(Some(&root));
    let selected = peer.ask(call(
        false,
        json!(1),
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
    ));
    tool(&selected, None, true);
    let root_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .root_key;
    let home = peer._home.path().to_path_buf();
    let index = |size: Option<u64>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_trellis"));
        command
            .arg("index")
            .arg("--workspace")
            .arg(&root)
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", home.join("cache"))
            .env("XDG_DATA_HOME", home.join("data"));
        if let Some(size) = size {
            command.arg("--max-file-bytes").arg(size.to_string());
        }
        let output = bounded_output(command);
        assert!(
            output.status.success(),
            "index: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        result["publishedRevision"].clone()
    };
    let first_pin = index(None);
    let deadline = tokio::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut state = CausalReadiness::default();
    let mut records = 0;
    while !state.settled() {
        records += 1;
        assert!(records <= 24, "initial watcher witness event budget");
        let event = next_causal_event(&listener, deadline).await;
        assert_eq!(event["rootKey"], root_key);
        state.observe(&event, &first_pin);
    }
    let original = state.lineage.clone().unwrap();
    let original_ordinal = state.lineage_ordinal.unwrap();
    let next_pin = index(Some(4096));
    assert_eq!(next_pin["indexGeneration"], first_pin["indexGeneration"]);
    // The CLI's published pin alone does not prove its selected option reached
    // the store. Inspect this fixture's sole index.db read-only immediately
    // after completion, before consuming the replacement watcher events.
    let mut dirs = vec![home.clone()];
    let mut index_db = None;
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.file_name().is_some_and(|name| name == "index.db") {
                assert!(
                    index_db.replace(path).is_none(),
                    "fixture has multiple indexes"
                );
            } else if path.is_dir() {
                dirs.push(path);
            }
        }
    }
    let db = rusqlite::Connection::open_with_flags(
        index_db.expect("selected index.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let options: String = db
        .query_row(
            "SELECT reconcile_options FROM index_metadata WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let options: Value = serde_json::from_str(&options).unwrap();
    assert_eq!(options["maxFileBytes"], 4096);
    drop(db);
    let deadline = tokio::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut observed = std::collections::HashSet::new();
    while !state.settled() || state.lineage_ordinal == Some(original_ordinal) {
        records += 1;
        assert!(records <= 24, "replacement watcher witness event budget");
        let event = next_causal_event(&listener, deadline).await;
        assert_eq!(event["rootKey"], root_key);
        let ordinal = event["lineageOrdinal"].as_u64().unwrap();
        let lineage = causal_lineage(&event);
        if ordinal > original_ordinal {
            assert_eq!(
                lineage.0, original.0,
                "option changes retain the same startup reconciliation owner"
            );
            assert_ne!(
                lineage.1, original.1,
                "replacement must mint a new watch epoch"
            );
            observed.insert(event["kind"].as_str().unwrap().to_owned());
        }
        state.observe(&event, &next_pin);
    }
    assert!(
        observed.contains("H_READY"),
        "new epoch needs its own startup reconciliation-ready snapshot"
    );
    assert!(
        observed.contains("WATCH_PENDING"),
        "new watcher needs a full wake"
    );
    assert!(
        observed.contains("WATCH_ACK"),
        "new watcher must acknowledge the full wake"
    );
    let final_reply = peer.ask(call(
        false,
        json!(2),
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
    ));
    tool(&final_reply, None, true);
    assert_eq!(
        final_reply["result"]["structuredContent"]["workspace"],
        selected["result"]["structuredContent"]["workspace"]
    );
    assert!(final_reply["result"]["structuredContent"]["catchingUp"].is_boolean());
    assert!(peer.finish().0.success());
}

/// Synthetic event ordering checks the fixture collector without a daemon.
/// Explicit event fields keep each source/epoch transition visible at the call site.
#[allow(clippy::too_many_arguments)]
fn causal_fixture_event(
    stream: uuid::Uuid,
    seq: u64,
    ordinal: u64,
    owner: uuid::Uuid,
    epoch: uuid::Uuid,
    kind: &str,
    pin: &Value,
    generation: u64,
) -> Value {
    json!({"kind":kind,"rootKey":"fixture","streamId":stream.to_string(),
      "streamSeq":seq,"lineageOrdinal":ordinal,
      "ownerIncarnation":owner.to_string(),"watchEpoch":epoch.to_string(),"pin":pin,
      "watchGeneration":generation,"queueEmpty":kind == "WATCH_ACK"})
}

#[test]
fn causal_witness_ignores_late_retired_epoch_and_requires_successor_startup() {
    let stream = uuid::Uuid::new_v4();
    let owner = uuid::Uuid::new_v4();
    let old = uuid::Uuid::new_v4();
    let next = uuid::Uuid::new_v4();
    let pin = json!({"indexGeneration":uuid::Uuid::new_v4().to_string(),"indexRevision":3});
    let mut state = CausalReadiness::default();
    assert!(state.observe(
        &causal_fixture_event(stream, 1, 1, owner, old, "WATCH_PENDING", &pin, 1),
        &pin
    ));
    assert!(state.observe(
        &causal_fixture_event(stream, 2, 1, owner, old, "H_READY", &pin, 1),
        &pin
    ));
    assert!(state.observe(
        &causal_fixture_event(stream, 3, 1, owner, old, "WATCH_ACK", &pin, 1),
        &pin
    ));
    assert!(state.settled());
    assert!(state.observe(
        &causal_fixture_event(stream, 4, 2, owner, next, "WATCH_PENDING", &pin, 1),
        &pin
    ));
    assert!(!state.settled());
    // An old epoch arrives after the new event. It consumes the stream sequence
    // but cannot reset the new epoch or certify readiness with stale startup reconciliation/ACK.
    assert!(!state.observe(
        &causal_fixture_event(stream, 5, 1, owner, old, "WATCH_ACK", &pin, 2),
        &pin
    ));
    assert_eq!(state.lineage_ordinal, Some(2));
    assert!(state.observe(
        &causal_fixture_event(stream, 6, 2, owner, next, "WATCH_ACK", &pin, 1),
        &pin
    ));
    assert!(
        !state.settled(),
        "ACK without successor startup reconciliation is insufficient"
    );
    assert!(state.observe(
        &causal_fixture_event(stream, 7, 2, owner, next, "H_READY", &pin, 1),
        &pin
    ));
    assert!(state.settled());
    assert!(state.observe(
        &causal_fixture_event(stream, 8, 2, owner, next, "WATCH_PENDING", &pin, 2),
        &pin
    ));
    assert!(!state.settled(), "post-ACK hint reopens catching up");
    assert!(state.observe(
        &causal_fixture_event(stream, 9, 2, owner, next, "WATCH_ACK", &pin, 2),
        &pin
    ));
    assert!(state.settled());
}

#[test]
fn causal_witness_rejects_ack_without_same_lineage_pending() {
    let stream = uuid::Uuid::new_v4();
    let owner = uuid::Uuid::new_v4();
    let epoch = uuid::Uuid::new_v4();
    let pin = json!({"indexGeneration":uuid::Uuid::new_v4().to_string(),"indexRevision":3});
    let mut state = CausalReadiness::default();
    assert!(state.observe(
        &causal_fixture_event(stream, 1, 1, owner, epoch, "H_READY", &pin, 1),
        &pin
    ));
    assert!(!state.settled());
    let ack_only = causal_fixture_event(stream, 2, 1, owner, epoch, "WATCH_ACK", &pin, 1);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || state.observe(&ack_only, &pin)
        ))
        .is_err()
    );
    let mut state = CausalReadiness::default();
    assert!(state.observe(
        &causal_fixture_event(stream, 1, 1, owner, epoch, "WATCH_PENDING", &pin, 1),
        &pin
    ));
    let wrong_generation = causal_fixture_event(stream, 2, 1, owner, epoch, "WATCH_ACK", &pin, 2);
    assert!(
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || state.observe(&wrong_generation, &pin)
        ))
        .is_err()
    );
}

#[test]
fn causal_witness_rejects_missing_or_duplicate_stream_event() {
    let stream = uuid::Uuid::new_v4();
    let owner = uuid::Uuid::new_v4();
    let epoch = uuid::Uuid::new_v4();
    let pin = json!({"indexGeneration":uuid::Uuid::new_v4().to_string(),"indexRevision":1});
    let mut state = CausalReadiness::default();
    let first = causal_fixture_event(stream, 1, 1, owner, epoch, "H_READY", &pin, 1);
    assert!(state.observe(&first, &pin));
    let duplicate =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.observe(&first, &pin)));
    assert!(duplicate.is_err());
    let mut state = CausalReadiness::default();
    assert!(state.observe(&first, &pin));
    let gap = causal_fixture_event(stream, 3, 1, owner, epoch, "WATCH_ACK", &pin, 1);
    let missing =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| state.observe(&gap, &pin)));
    assert!(missing.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_reports_actual_degraded_watcher_on_selected_snapshot() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("degraded.js"), "function preserved() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(31, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((snapshot, _)) = runtime.evidence_response()
                && snapshot.finish(()).is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("mandatory startup reconciliation did not publish");
    let scheduler = runtime.browser_scheduler().unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if scheduler.submit_watch_event_for_tests(Err(notify::Error::generic(
                "degraded watcher fixture",
            ))) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("watcher was not installed after H");
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if runtime.mcp_watcher_degraded().unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("watcher did not observe the error");
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let daemon_handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity),
            server,
            registry,
            31,
            daemon_handle,
        )
    });
    writeln!(
        client,
        "{}",
        modern(
            json!(41),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(client.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let answer: Value = serde_json::from_str(&line).unwrap();
    tool(&answer, None, true);
    let envelope = &answer["result"]["structuredContent"];
    assert_eq!(envelope["data"]["watcherDegraded"], true);
    assert_eq!(envelope["data"]["indexState"], "ready");
    assert!(envelope["evidenceBasis"]["indexRevision"].as_u64().unwrap() > 0);
    assert_eq!(envelope["data"]["languages"].as_array().unwrap().len(), 4);
    drop(client);
    session.join().unwrap().unwrap();
}

/// The call deadline bounds the reply, measured from admission. A selected
/// read still running at the deadline gets `deadline_exceeded` then, from the
/// call's timer, and its late successful result is discarded, never written.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_replies_at_its_deadline_while_the_read_is_still_running() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function slow() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(34, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response()
                && response.finish(()).is_ok()
                && !catching_up
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("initial startup reconciliation did not finish");
    // Hold the next selected read once it has started.
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel::<()>(1);
    runtime.set_mcp_read_hook_for_tests(move || {
        entered_tx.send(()).unwrap();
        // Held until released, or until the test ends and drops the sender:
        // never released by a timer.
        let _ = release_rx.recv();
    });
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let daemon_handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_deadline_for_tests(Duration::from_millis(300)),
            server,
            registry,
            34,
            daemon_handle,
        )
    });
    writeln!(
        client,
        "{}",
        modern(
            json!(61),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    entered_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("the selected read never started");
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    // The read is still held, so only the call's timer can have answered.
    let response: Value = serde_json::from_str(&line).unwrap();
    release_tx.send(()).unwrap();
    tool(&response, Some("deadline_exceeded"), true);
    assert_eq!(
        response["result"]["structuredContent"]["error"]["retryable"],
        true
    );
    // The released read now succeeds in the background; its result must not
    // reach the client.
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    let mut stray = String::new();
    assert!(
        reader.read_line(&mut stray).is_err(),
        "late result was written after the deadline reply: {stray}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_reports_published_watcher_health_while_an_inventory_holds_the_watcher() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function held() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(32, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response()
                && response.finish(()).is_ok()
                && !catching_up
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("initial startup reconciliation did not finish");
    let scheduler = runtime.browser_scheduler().unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let holder = std::thread::spawn(move || {
        scheduler.hold_worker_step_for_tests(|| {
            entered_tx.send(()).unwrap();
            release_rx
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap();
        })
    });
    entered_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let daemon_handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity),
            server,
            registry,
            32,
            daemon_handle,
        )
    });
    writeln!(
        client,
        "{}",
        modern(
            json!(42),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(client.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    // Health is published, not sampled under the watcher lock. Describe
    // answers while the gate is still held, so it never waited for the
    // inventory; success, not a deadline refusal, proves it.
    release_tx.send(()).unwrap();
    tool(&response, None, true);
    assert_eq!(
        response["result"]["structuredContent"]["data"]["watcherDegraded"], false,
        "{response}"
    );
    holder.join().unwrap();
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_serves_valid_prior_head_while_startup_reconciliation_is_paused() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("prior.js"), "function prior() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(43, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response()
                && response.finish(()).is_ok()
                && !catching_up
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("initial startup reconciliation did not finish");
    let (response, _) = runtime.evidence_response().unwrap();
    let prior = response.status().unwrap().revision;
    response.finish(()).unwrap();
    drop(response);
    registry.disconnect(43);
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while !registry.release(&identity.root_key).unwrap() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("checkout was not released");
    fs::write(root.join("prior.js"), "function later() {}\n").unwrap();
    registry.attach_launch(44, &identity).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx
            .lock()
            .unwrap()
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    }));
    let _resumed = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if entered_rx.try_recv().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("H did not pause");
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let daemon_handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity),
            server,
            registry,
            44,
            daemon_handle,
        )
    });
    writeln!(
        client,
        "{}",
        modern(
            json!(45),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(client.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let answer: Value = serde_json::from_str(&line).unwrap();
    tool(&answer, None, true);
    let envelope = &answer["result"]["structuredContent"];
    assert_eq!(envelope["catchingUp"], true);
    assert_eq!(envelope["data"]["indexState"], "ready");
    assert_eq!(
        envelope["evidenceBasis"],
        json!({
        "indexGeneration":prior.index_generation.to_string(),
        "indexRevision":prior.index_revision})
    );
    assert_eq!(envelope["data"]["languages"].as_array().unwrap().len(), 4);
    resume_tx.send(()).unwrap();
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_cold_initial_startup_reports_indexing_without_prior_coverage() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("cold.js"), "function cold() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(46, &identity).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx
            .lock()
            .unwrap()
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    }));
    let _runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if entered_rx.try_recv().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("initial startup reconciliation did not pause");
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let daemon_handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity),
            server,
            registry,
            46,
            daemon_handle,
        )
    });
    writeln!(
        client,
        "{}",
        modern(
            json!(47),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(client.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let answer: Value = serde_json::from_str(&line).unwrap();
    tool(&answer, None, true);
    let envelope = &answer["result"]["structuredContent"];
    assert_eq!(envelope["catchingUp"], true);
    assert_eq!(envelope["data"]["indexState"], "indexing");
    assert_eq!(
        envelope["data"]["progress"],
        json!({"completed":0,"total":null})
    );
    assert!(envelope["evidenceBasis"].is_null());
    assert_eq!(envelope["data"]["languages"], json!([]));
    let mut expected = fixture()["expected"]["describe"].clone();
    expected["requestId"] = json!(47);
    expected["data"]["workspaceLabel"] = json!("checkout");
    expected["workspace"] = envelope["workspace"].clone();
    expected["catchingUp"] = json!(true);
    assert_eq!(*envelope, expected);
    assert_eq!(
        answer["result"]["content"][0]["text"],
        serde_json::to_string(&expected).unwrap()
    );
    resume_tx.send(()).unwrap();
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn describe_counts_persisted_selected_and_unselected_native_coverage() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    for (path, text) in [
        ("a.java", "class A {}\n"),
        ("b.rs", "fn broken( {\n"),
        ("c.py", "def complete(): pass\n"),
        ("d.js", "function unselected() {}\n"),
    ] {
        fs::write(root.join(path), text).unwrap();
    }
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    let baseline = store.index_baseline().unwrap();
    assert_eq!(baseline.index_revision, 0);
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &writer, baseline, &cancel)
        .unwrap();
    store
        .mcp_set_coverage_for_tests(&writer, "c.py", true, "failed")
        .unwrap();
    store
        .mcp_set_coverage_for_tests(&writer, "d.js", false, "unsupported")
        .unwrap();
    drop(writer);
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(48, &identity).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    // Installed before activation starts the worker, so startup cannot pass it first.
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx
            .lock()
            .unwrap()
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    }));
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if entered_rx.try_recv().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let status = runtime.evidence_response().and_then(|(response, catching_up)| {
            let revision = response.status()?.revision;
            response.finish(revision).map(|revision| (revision, catching_up))
        });
        panic!(
            "H did not pause after persisted fixture: published={pin:?}, last_error={:?}, retry_waiting={}, catching_up={}, selected_status={status:?}",
            runtime.reconciliation_error(),
            runtime.retry_waiting_for_tests(),
            runtime.catching_up(),
        );
    });
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let daemon_handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity),
            server,
            registry,
            48,
            daemon_handle,
        )
    });
    writeln!(
        client,
        "{}",
        modern(
            json!(49),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    let mut line = String::new();
    BufReader::new(client.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    let answer: Value = serde_json::from_str(&line).unwrap();
    tool(&answer, None, true);
    let envelope = &answer["result"]["structuredContent"];
    assert_eq!(envelope["data"]["indexState"], "ready");
    assert_eq!(envelope["catchingUp"], true);
    assert_eq!(
        envelope["evidenceBasis"],
        json!({
        "indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision})
    );
    let rows = &envelope["data"]["languages"];
    for (index, language, selected, complete, partial, failed, unselected) in [
        (0, "java", 1, 1, 0, 0, 0),
        (1, "rust", 1, 0, 1, 0, 0),
        (2, "python", 1, 0, 0, 1, 0),
        (3, "javascript", 0, 0, 0, 0, 1),
    ] {
        assert_eq!(rows[index]["language"], language);
        assert_eq!(
            rows[index]["coverage"],
            json!({"selected":selected,"complete":complete,
            "partial":partial,"failed":failed,"unselected":unselected})
        );
    }
    assert_eq!(envelope["partial"], true);
    resume_tx.send(()).unwrap();
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_retries_direct_sqlite_contention_on_fresh_selected_snapshot() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("busy.js"), "function stable() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(50, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response()
                && response.finish(()).is_ok()
                && !catching_up
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("initial startup reconciliation did not settle");
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let daemon_handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity),
            server,
            registry,
            50,
            daemon_handle,
        )
    });
    let describe = |client: &mut UnixStream, id: i64| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}})
            )
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    runtime.mcp_busy_reads_for_tests(2).unwrap();
    let recovered = describe(&mut client, 51);
    tool(&recovered, None, true);
    assert_eq!(
        recovered["result"]["structuredContent"]["data"]["indexState"],
        "ready"
    );
    assert_eq!(
        recovered["result"]["structuredContent"]["data"]["languages"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    assert!(
        recovered["result"]["structuredContent"]["evidenceBasis"]["indexRevision"]
            .as_u64()
            .unwrap()
            > 0
    );
    // Selected reads stay busy for good, so only the call deadline can end
    // this call. Advance the checkout's MCP clock past it instead of waiting:
    // a server that waited for the reads instead would hang here.
    runtime.mcp_busy_reads_for_tests(usize::MAX).unwrap();
    runtime.advance_mcp_clock_for_tests(trellis::mcp::CALL_DEADLINE + Duration::from_secs(1));
    let expired = describe(&mut client, 52);
    tool(&expired, Some("deadline_exceeded"), true);
    let envelope = &expired["result"]["structuredContent"];
    assert!(envelope.get("data").is_none());
    assert_eq!(envelope["error"]["retryable"], true);
    let codes = trellis::mcp::catalog::list(true)["tools"][0]["outputSchema"]["$defs"]["ResolvedFailureCode"]["enum"].as_array().unwrap().clone();
    assert!(codes.contains(&envelope["error"]["code"]));
    runtime.mcp_busy_reads_for_tests(0).unwrap();
    drop(client);
    session.join().unwrap().unwrap();
}

#[test]
fn invalid_calls_and_non_w1_stubs_never_open_derived_databases() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    for legacy in [false, true] {
        let mut peer = Peer::start(Some(&root));
        if legacy {
            ready_legacy(&mut peer);
        } else {
            assert_eq!(
                peer.ask(modern(json!(1), "server/discover", json!({})))["result"]["resultType"],
                "complete"
            );
        }
        no_db_under(peer._home.path());
        for (index, name) in fixture()["names"].as_array().unwrap().iter().enumerate() {
            let invalid = peer.ask(call(
                legacy,
                json!(10 + index),
                name.as_str().unwrap(),
                json!({"schemaVersion":1,"unexpected":"reject"}),
            ));
            tool(&invalid, Some("invalid_request"), !legacy);
            no_db_under(peer._home.path());
        }
        for (offset, name) in fixture()["names"]
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
            .skip(1)
        {
            let reply = peer.ask(call(
                legacy,
                json!(20 + offset),
                name.as_str().unwrap(),
                fixture()["validArguments"][offset].clone(),
            ));
            tool(&reply, Some("index_not_ready"), !legacy);
            no_db_under(peer._home.path());
        }
        assert!(peer.finish().0.success());
    }
}

/// A read gate that holds every MCP selected read until opened.
#[derive(Clone, Default)]
struct ReadGate(std::sync::Arc<(std::sync::Mutex<(usize, bool)>, std::sync::Condvar)>);
impl ReadGate {
    fn hook(&self) -> std::sync::Arc<dyn Fn() + Send + Sync> {
        let gate = self.clone();
        std::sync::Arc::new(move || {
            let (state, changed) = &*gate.0;
            let mut held = state.lock().unwrap();
            held.0 += 1;
            changed.notify_all();
            while !held.1 {
                held = changed.wait(held).unwrap();
            }
        })
    }
    /// Wait (bounded only by the hang backstop) until `count` reads are held.
    fn await_held(&self, count: usize) {
        let (state, changed) = &*self.0;
        let held = state.lock().unwrap();
        let (held, timeout) = changed
            .wait_timeout_while(held, trellis::testing::HANG_BACKSTOP, |held| held.0 < count)
            .unwrap();
        assert!(!timeout.timed_out(), "only {} reads were held", held.0);
    }
    fn open(&self) {
        let (state, changed) = &*self.0;
        state.lock().unwrap().1 = true;
        changed.notify_all();
    }
}
impl Drop for ReadGate {
    fn drop(&mut self) {
        // Never strand held readers when a test fails.
        if std::sync::Arc::strong_count(&self.0) == 1 {
            self.open();
        }
    }
}

/// An activated, settled checkout and its daemon registry.
async fn ready_registry(
    temp: &Path,
    session: u64,
) -> (
    trellis::store::topology::WorkspaceIdentity,
    std::sync::Arc<tokio::sync::Mutex<trellis::daemon::registry::CheckoutRegistry>>,
    std::sync::Arc<trellis::daemon::registry::CheckoutRuntime>,
) {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let root = checkout(temp);
    fs::write(root.join("a.js"), "function held() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology = TopologyRoots::isolated_for_tests(temp.join("cache"), temp.join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(session, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response()
                && response.finish(()).is_ok()
                && !catching_up
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("startup reconciliation did not finish");
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    (
        identity,
        std::sync::Arc::new(tokio::sync::Mutex::new(registry)),
        runtime,
    )
}

/// One daemon-socket MCP connection served on a thread.
fn socket_session(
    identity: &trellis::store::topology::WorkspaceIdentity,
    registry: &std::sync::Arc<tokio::sync::Mutex<trellis::daemon::registry::CheckoutRegistry>>,
    session: u64,
    deadline: Option<Duration>,
) -> (
    std::os::unix::net::UnixStream,
    BufReader<std::os::unix::net::UnixStream>,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    hooked_session(identity, registry, session, deadline, Default::default())
}

fn hooked_session(
    identity: &trellis::store::topology::WorkspaceIdentity,
    registry: &std::sync::Arc<tokio::sync::Mutex<trellis::daemon::registry::CheckoutRegistry>>,
    session: u64,
    deadline: Option<Duration>,
    hooks: trellis::mcp::TransportHooksForTests,
) -> (
    std::os::unix::net::UnixStream,
    BufReader<std::os::unix::net::UnixStream>,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    let (client, server) = std::os::unix::net::UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let mut workspace = trellis::mcp::OpenedWorkspace::new(
        trellis::store::topology::WorkspaceIdentity::discover(Some(&identity.root), &identity.root)
            .unwrap()
            .attach_marker()
            .unwrap(),
    );
    if let Some(deadline) = deadline {
        workspace = workspace.with_call_deadline_for_tests(deadline);
    }
    let workspace = workspace.with_transport_hooks_for_tests(hooks);
    let registry = registry.clone();
    let handle = tokio::runtime::Handle::current();
    let thread = std::thread::spawn(move || {
        trellis::mcp::run_socket(workspace, server, registry, session, handle)
    });
    let reader = BufReader::new(client.try_clone().unwrap());
    (client, reader, thread)
}

fn describe_call(id: i64) -> Value {
    modern(
        json!(id),
        "tools/call",
        json!({"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}),
    )
}

fn next_reply(reader: &mut BufReader<std::os::unix::net::UnixStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    serde_json::from_str(&line).unwrap()
}

/// Decision 0009 §7: four admitted calls on a connection run at once; a fifth
/// is refused at once, without queueing, in the pre-resolution shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn four_calls_run_concurrently_and_a_fifth_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 71).await;
    let gate = ReadGate::default();
    runtime.set_mcp_read_gate_for_tests(gate.hook());
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 71, None);
    for id in 1..=4 {
        writeln!(client, "{}", describe_call(id)).unwrap();
    }
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(4))
        .await
        .unwrap();
    writeln!(client, "{}", describe_call(5)).unwrap();
    let refused = next_reply(&mut reader);
    assert_eq!(refused["id"], 5, "{refused}");
    let envelope = &refused["result"]["structuredContent"];
    assert_eq!(envelope["error"]["code"], "too_many_requests", "{refused}");
    assert_eq!(envelope["error"]["retryable"], true);
    assert_eq!(envelope["error"]["attemptedWorkspace"]["kind"], "path");
    assert!(envelope.get("workspace").is_none() && envelope.get("catchingUp").is_none());
    gate.open();
    let mut answered: Vec<i64> = (0..4)
        .map(|_| {
            let reply = next_reply(&mut reader);
            tool(&reply, None, true);
            reply["id"].as_i64().unwrap()
        })
        .collect();
    answered.sort_unstable();
    assert_eq!(answered, vec![1, 2, 3, 4]);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// Decision 0009 §7: a call worker keeps its permit until its thread exits,
/// even after its call answered deadline_exceeded, across connections; the
/// daemon-wide budget then refuses new calls until workers finish.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_workers_keep_their_permits_across_connections() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 72).await;
    let gate = ReadGate::default();
    runtime.set_mcp_read_gate_for_tests(gate.hook());
    let mut connections = Vec::new();
    for connection in 0..4 {
        let (mut client, mut reader, session) =
            socket_session(&identity, &registry, 72, Some(Duration::from_millis(300)));
        for call in 0..4 {
            writeln!(client, "{}", describe_call(connection * 10 + call)).unwrap();
        }
        for _ in 0..4 {
            let reply = next_reply(&mut reader);
            assert_eq!(
                reply["result"]["structuredContent"]["error"]["code"], "deadline_exceeded",
                "{reply}"
            );
        }
        connections.push((client, reader, session));
    }
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(16))
        .await
        .unwrap();
    assert_eq!(trellis::mcp::live_call_workers_for_tests(), 16);
    // A new connection: every permit is still held by a timed-out worker.
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 72, None);
    writeln!(client, "{}", describe_call(99)).unwrap();
    let refused = next_reply(&mut reader);
    assert_eq!(
        refused["result"]["structuredContent"]["error"]["code"], "too_many_requests",
        "{refused}"
    );
    gate.open();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while trellis::mcp::live_call_workers_for_tests() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed-out workers did not exit");
    writeln!(client, "{}", describe_call(100)).unwrap();
    tool(&next_reply(&mut reader), None, true);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    for (client, reader, session) in connections {
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
}

/// Decision 0009 §7: a cancelled call writes nothing and its worker exits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_call_writes_nothing_and_its_worker_exits() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 73).await;
    let gate = ReadGate::default();
    runtime.set_mcp_read_gate_for_tests(gate.hook());
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 73, None);
    writeln!(client, "{}", describe_call(1)).unwrap();
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    writeln!(
        client,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}})
    )
    .unwrap();
    // A later call is answered, so the cancellation has been settled.
    let tools_list = modern(json!(2), "tools/list", json!({}));
    writeln!(client, "{tools_list}").unwrap();
    let listed = next_reply(&mut reader);
    assert_eq!(listed["id"], 2, "the cancelled call answered: {listed}");
    gate.open();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while trellis::mcp::live_call_workers_for_tests() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the cancelled worker did not exit");
    client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut stray = String::new();
    assert!(
        reader.read_line(&mut stray).is_err(),
        "the cancelled call wrote: {stray}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// A one-shot pause for one call ID at the coordinator's send decision.
fn hold_send_decision(
    id: i64,
) -> (
    trellis::mcp::CallHookForTests,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::SyncSender<()>,
) {
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    let hook = std::sync::Arc::new(move |call: &Value| {
        if call.as_i64() == Some(id) {
            let _ = entered_tx.send(());
            // A dropped sender (test failure) ends the hold.
            let _ = release_rx.lock().unwrap().recv();
        }
    });
    (hook, entered_rx, release_tx)
}

/// Signals each frame the connection's reader completes.
fn frames_read() -> (
    std::sync::Arc<dyn Fn() + Send + Sync>,
    std::sync::mpsc::Receiver<std::time::Instant>,
) {
    let (read_tx, read_rx) = std::sync::mpsc::channel();
    let read_tx = std::sync::Mutex::new(read_tx);
    let hook = std::sync::Arc::new(move || {
        let _ = read_tx.lock().unwrap().send(std::time::Instant::now());
    });
    (hook, read_rx)
}

/// Decision 0009 §7: a call's deadline starts when its frame is read, even if
/// the coordinator is held deciding an earlier call; a call that expired
/// meanwhile answers deadline_exceeded (pre-resolution shape), and the held
/// call's own decision rechecks its deadline after the final check.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_expires_from_its_read_while_the_coordinator_is_held() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, _runtime) = ready_registry(temp.path(), 81).await;
    let (send_decision, entered, release) = hold_send_decision(1);
    let (frame_read, read) = frames_read();
    let deadline = Duration::from_millis(300);
    let hooks = trellis::mcp::TransportHooksForTests {
        frame_read: Some(frame_read),
        send_decision: Some(send_decision),
        ..Default::default()
    };
    let (mut client, mut reader, session) =
        hooked_session(&identity, &registry, 81, Some(deadline), hooks);
    writeln!(client, "{}", describe_call(1)).unwrap();
    read.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    entered
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("call 1 did not reach its send decision");
    writeln!(client, "{}", describe_call(2)).unwrap();
    let accepted = read.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    // Wait out call 2's own deadline (measured from its read) while held.
    tokio::time::sleep_until((accepted + deadline).into()).await;
    release.send(()).unwrap();
    for id in [1, 2] {
        let reply = next_reply(&mut reader);
        assert_eq!(reply["id"], id, "{reply}");
        let envelope = &reply["result"]["structuredContent"];
        assert_eq!(envelope["error"]["code"], "deadline_exceeded", "{reply}");
        if id == 2 {
            assert!(
                envelope["error"]["attemptedWorkspace"].is_object(),
                "{reply}"
            );
            assert!(envelope.get("workspace").is_none(), "{reply}");
        }
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// Decision 0009 §7: once read admission sampled catchingUp, a deadline
/// answer keeps the resolved shape.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deadline_after_read_admission_keeps_the_resolved_shape() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 82).await;
    let gate = ReadGate::default();
    runtime.set_mcp_admitted_gate_for_tests(gate.hook());
    let (mut client, mut reader, session) =
        socket_session(&identity, &registry, 82, Some(Duration::from_millis(300)));
    writeln!(client, "{}", describe_call(1)).unwrap();
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    let reply = next_reply(&mut reader);
    gate.open();
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["error"]["code"], "deadline_exceeded", "{reply}");
    assert_eq!(
        envelope["workspace"],
        json!(identity.root),
        "post-admission deadline lost its resolved shape: {reply}"
    );
    assert!(envelope["catchingUp"].is_boolean(), "{reply}");
    assert!(
        envelope["error"].get("attemptedWorkspace").is_none(),
        "{reply}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #127 at the daemon (Decision 0009 §7): a cancel the reader completed
/// before the send decision, but has not yet published, still suppresses the
/// unsent result: the decision waits for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_completed_before_the_decision_wins_even_if_unpublished() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, _runtime) = ready_registry(temp.path(), 83).await;
    let (send_decision, entered, release) = hold_send_decision(1);
    // The reader holds the second frame (the cancel) after completing and
    // counting it, before publishing it.
    let (reader_held_tx, reader_held) = std::sync::mpsc::sync_channel(1);
    let (publish_tx, publish_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let publish_rx = std::sync::Mutex::new(publish_rx);
    let frames = std::sync::atomic::AtomicUsize::new(0);
    let frame_read: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
        if frames.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            let _ = reader_held_tx.send(());
            let _ = publish_rx.lock().unwrap().recv();
        }
    });
    let (waiting_tx, waiting) = std::sync::mpsc::sync_channel(4);
    let settle_waiting: std::sync::Arc<dyn Fn() + Send + Sync> = std::sync::Arc::new(move || {
        let _ = waiting_tx.try_send(());
    });
    let hooks = trellis::mcp::TransportHooksForTests {
        frame_read: Some(frame_read),
        send_decision: Some(send_decision),
        settle_waiting: Some(settle_waiting),
        ..Default::default()
    };
    let (mut client, reader, session) = hooked_session(&identity, &registry, 83, None, hooks);
    let replies = {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut reader = reader;
        std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let _ = tx.send(serde_json::from_str::<Value>(&line).unwrap());
            }
        });
        rx
    };
    writeln!(client, "{}", describe_call(1)).unwrap();
    entered
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("call 1 did not reach its send decision");
    writeln!(
        client,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}})
    )
    .unwrap();
    reader_held
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("the reader did not complete the cancel");
    release.send(()).unwrap();
    // The decision must wait for the completed cancel: it is never answered.
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    loop {
        if waiting.try_recv().is_ok() {
            break;
        }
        if let Ok(reply) = replies.try_recv() {
            panic!("decided without the completed cancel: {reply}");
        }
        assert!(std::time::Instant::now() < deadline, "no decision");
        std::thread::sleep(Duration::from_millis(1));
    }
    publish_tx.send(()).unwrap();
    writeln!(client, "{}", modern(json!(2), "tools/list", json!({}))).unwrap();
    let next = replies
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    assert_eq!(next["id"], 2, "the cancelled call was answered: {next}");
    // The reply thread holds a clone: end input explicitly, then close ours.
    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(client);
    session.join().unwrap().unwrap();
}

/// Decision 0009 §7: requests beyond the connection's bounds are refused at
/// once, application validation first: an invalid call stays
/// invalid_request; a fifth valid call gets too_many_requests; a 17th
/// outstanding non-call request gets -32000 too_many_requests.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn over_bound_requests_are_refused_after_validation() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 84).await;
    let (send_decision, entered, release) = hold_send_decision(1);
    let (frame_read, read) = frames_read();
    let hooks = trellis::mcp::TransportHooksForTests {
        frame_read: Some(frame_read),
        send_decision: Some(send_decision),
        ..Default::default()
    };
    let (mut client, mut reader, session) = hooked_session(&identity, &registry, 84, None, hooks);
    writeln!(client, "{}", describe_call(1)).unwrap();
    read.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    entered
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("call 1 did not reach its send decision");
    // Held reads keep calls 2..5 outstanding once dispatched.
    let gate = ReadGate::default();
    runtime.set_mcp_read_gate_for_tests(gate.hook());
    let mut burst = String::new();
    for id in 2..=5 {
        burst.push_str(&format!("{}\n", describe_call(id)));
    }
    burst.push_str(&format!(
        "{}\n",
        modern(
            json!(6),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{}})
        )
    ));
    burst.push_str(&format!("{}\n", describe_call(7)));
    for id in 101..=117 {
        burst.push_str(&format!("{}\n", modern(json!(id), "tools/list", json!({}))));
    }
    client.write_all(burst.as_bytes()).unwrap();
    // Every burst frame is complete before the decision: one fence handles
    // them all, so 17 non-call requests are live at once.
    for _ in 0..burst.lines().count() {
        read.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    }
    release.send(()).unwrap();
    let mut seen = std::collections::BTreeMap::new();
    while !(seen.contains_key("6") && seen.contains_key("7") && seen.contains_key("117")) {
        let reply = next_reply(&mut reader);
        seen.insert(reply["id"].to_string(), reply);
    }
    gate.open();
    let invalid = &seen["6"]["result"]["structuredContent"];
    assert_eq!(
        invalid["error"]["code"], "invalid_request",
        "validation lost to capacity: {invalid}"
    );
    let fifth = &seen["7"]["result"]["structuredContent"];
    assert_eq!(fifth["error"]["code"], "too_many_requests", "{fifth}");
    let seventeenth = &seen["117"];
    assert_eq!(seventeenth["error"]["code"], -32000, "{seventeenth}");
    assert_eq!(seventeenth["error"]["data"]["code"], "too_many_requests");
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// Decision 0009 §7: an immediate call outcome still unsent at the deadline
/// answers deadline_exceeded.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_immediate_outcome_unsent_at_the_deadline_expires() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, _runtime) = ready_registry(temp.path(), 85).await;
    let deadline = Duration::from_millis(300);
    let (frame_read, read) = frames_read();
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    let hooks = trellis::mcp::TransportHooksForTests {
        frame_read: Some(frame_read),
        immediate_decision: Some(std::sync::Arc::new(move |_: &Value| {
            let _ = entered_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
        })),
        ..Default::default()
    };
    let (mut client, mut reader, session) =
        hooked_session(&identity, &registry, 85, Some(deadline), hooks);
    writeln!(
        client,
        "{}",
        modern(
            json!(1),
            "tools/call",
            json!({"name":"trellis_workspace_describe","arguments":{}})
        )
    )
    .unwrap();
    let accepted = read.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    entered
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    tokio::time::sleep_until((accepted + deadline).into()).await;
    release_tx.send(()).unwrap();
    let reply = next_reply(&mut reader);
    assert_eq!(
        reply["result"]["structuredContent"]["error"]["code"], "deadline_exceeded",
        "an unsent outcome survived its deadline: {reply}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

fn cancel_frame(id: i64) -> Value {
    json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})
}

/// Decision 0009 §7: capacity counts live calls only. With four calls held,
/// a cancel for one and a fifth call in the same burst admit the fifth.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fifth_call_after_a_cancellation_is_admitted() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 86).await;
    let gate = ReadGate::default();
    runtime.set_mcp_read_gate_for_tests(gate.hook());
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 86, None);
    for id in 1..=4 {
        writeln!(client, "{}", describe_call(id)).unwrap();
    }
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(4))
        .await
        .unwrap();
    write!(client, "{}\n{}\n", cancel_frame(1), describe_call(5)).unwrap();
    // The fifth call reaches its read: admitted, not refused.
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(5))
        .await
        .unwrap();
    gate.open();
    let mut answered: Vec<i64> = (0..4)
        .map(|_| {
            let reply = next_reply(&mut reader);
            assert!(
                reply["result"]["structuredContent"]["data"].is_object(),
                "{reply}"
            );
            reply["id"].as_i64().unwrap()
        })
        .collect();
    answered.sort_unstable();
    assert_eq!(answered, vec![2, 3, 4, 5]);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// Decision 0009 §7: a capacity refusal still unsent at the call's deadline
/// answers deadline_exceeded. Capacity here is the daemon-wide worker budget,
/// which timed-out workers keep, so it stays full past the call's deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capacity_refusal_unsent_at_the_deadline_expires() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 87).await;
    let gate = ReadGate::default();
    runtime.set_mcp_read_gate_for_tests(gate.hook());
    // Sixteen held workers on four other connections exhaust the budget.
    let mut held = Vec::new();
    for connection in 0..4 {
        let (mut client, reader, session) = socket_session(&identity, &registry, 87, None);
        for call in 0..4 {
            writeln!(client, "{}", describe_call(connection * 10 + call)).unwrap();
        }
        held.push((client, reader, session));
    }
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(16))
        .await
        .unwrap();
    let deadline = Duration::from_millis(500);
    let (frame_read, read) = frames_read();
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    let hooks = trellis::mcp::TransportHooksForTests {
        frame_read: Some(frame_read),
        immediate_decision: Some(std::sync::Arc::new(move |_: &Value| {
            let _ = entered_tx.send(());
            let _ = release_rx.lock().unwrap().recv();
        })),
        ..Default::default()
    };
    let (mut client, mut reader, session) =
        hooked_session(&identity, &registry, 87, Some(deadline), hooks);
    writeln!(client, "{}", describe_call(99)).unwrap();
    let accepted = read.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    entered
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    tokio::time::sleep_until((accepted + deadline).into()).await;
    release_tx.send(()).unwrap();
    let reply = next_reply(&mut reader);
    assert_eq!(
        reply["result"]["structuredContent"]["error"]["code"], "deadline_exceeded",
        "a capacity refusal was sent past its deadline: {reply}"
    );
    gate.open();
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    for (client, reader, session) in held {
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
}

/// Decision 0009 §7 / T00: a sustained stream of invalid calls is answered
/// invalid_request, each fence handles a bounded batch, and a call's reply
/// is decided while the stream continues.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn continuous_invalid_calls_stay_bounded_and_a_due_reply_is_decided() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 88).await;
    let gate = ReadGate::default();
    runtime.set_mcp_read_gate_for_tests(gate.hook());
    let largest = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = largest.clone();
    let hooks = trellis::mcp::TransportHooksForTests {
        fence_handled: Some(std::sync::Arc::new(move |handled| {
            observed.fetch_max(handled, std::sync::atomic::Ordering::AcqRel);
        })),
        ..Default::default()
    };
    let (client, reader, session) = hooked_session(&identity, &registry, 88, None, hooks);
    let mut writer = client.try_clone().unwrap();
    writeln!(writer, "{}", describe_call(0)).unwrap();
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flooding = stop.clone();
    let flood = std::thread::spawn(move || {
        let mut id = 1_000;
        while !flooding.load(std::sync::atomic::Ordering::Acquire) {
            let invalid = modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_workspace_describe","arguments":{}}),
            );
            if writeln!(writer, "{invalid}").is_err() {
                break;
            }
            id += 1;
        }
        id - 1_000
    });
    // Let the stream run, then release the held call while it continues.
    let mut reader = reader;
    let mut invalid = 0;
    while invalid < 2_000 {
        let reply = next_reply(&mut reader);
        assert_eq!(
            reply["result"]["structuredContent"]["error"]["code"], "invalid_request",
            "{reply}"
        );
        invalid += 1;
    }
    gate.open();
    loop {
        let reply = next_reply(&mut reader);
        if reply["id"] == 0 {
            assert!(
                reply["result"]["structuredContent"]["data"].is_object(),
                "{reply}"
            );
            break;
        }
    }
    // This side stops reading now: end the connection first, so a flood
    // write blocked behind unread replies fails instead of waiting forever.
    stop.store(true, std::sync::atomic::Ordering::Release);
    // The server can close first after deciding the due reply.
    if let Err(error) = client.shutdown(std::net::Shutdown::Both) {
        assert_eq!(error.kind(), std::io::ErrorKind::NotConnected);
    }
    let sent = flood.join().unwrap();
    assert!(sent > 2_000);
    let largest = largest.load(std::sync::atomic::Ordering::Acquire);
    assert!(
        largest <= 66,
        "a fence handled {largest} requests: not bounded by the input channel"
    );
    drop(reader);
    drop(client);
    // Replies to flood requests may still be unwritten when the client goes:
    // a hang-up ends the session cleanly.
    session.join().unwrap().unwrap();
}

/// A finished call's result never waits for the client's input: with the
/// coordinator held and the input window full of frames, the worker still
/// hands its result over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_result_never_waits_for_client_input() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 89).await;
    let gate = ReadGate::default();
    runtime.set_mcp_read_gate_for_tests(gate.hook());
    let (frame_read, read) = frames_read();
    let (held_tx, held) = std::sync::mpsc::sync_channel(1);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    let (done_tx, done) = std::sync::mpsc::sync_channel(4);
    let hooks = trellis::mcp::TransportHooksForTests {
        frame_read: Some(frame_read),
        // Hold the coordinator at call 2's decision.
        immediate_decision: Some(std::sync::Arc::new(move |id: &Value| {
            if id == &json!(2) {
                let _ = held_tx.send(());
                let _ = release_rx.lock().unwrap().recv();
            }
        })),
        worker_done: Some(std::sync::Arc::new(move || {
            let _ = done_tx.try_send(());
        })),
        ..Default::default()
    };
    let (mut client, mut reader, session) = hooked_session(&identity, &registry, 89, None, hooks);
    writeln!(client, "{}", describe_call(1)).unwrap();
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    writeln!(client, "{}", describe_call(2)).unwrap();
    held.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    // Fill the input window while the coordinator is held.
    let mut burst = String::new();
    for id in 100..140 {
        burst.push_str(&format!("{}\n", modern(json!(id), "tools/list", json!({}))));
    }
    client.write_all(burst.as_bytes()).unwrap();
    for _ in 0..34 {
        read.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    }
    // Call 1 finishes: its result is handed over without the coordinator.
    gate.open();
    done.recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("the worker's result waited for client input");
    release_tx.send(()).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    while !seen.contains(&1) {
        let reply = next_reply(&mut reader);
        seen.insert(reply["id"].as_i64().unwrap());
    }
    // Close every client handle: replies still being written then fail at
    // once. A shutdown alone need not unblock a full macOS socket buffer.
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// A client that hangs up while its reply is being written ends the session
/// cleanly (Ok): the write after the decision meets a closed peer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_hang_up_ends_the_session_cleanly() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, _runtime) = ready_registry(temp.path(), 90).await;
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let marked = finished.clone();
    let (at_write_tx, at_write) = std::sync::mpsc::sync_channel(1);
    let (closed_tx, closed_rx) = std::sync::mpsc::sync_channel::<()>(1);
    let closed_rx = std::sync::Mutex::new(closed_rx);
    let hooks = trellis::mcp::TransportHooksForTests {
        send_decision: Some(std::sync::Arc::new(move |_: &Value| {
            marked.store(true, std::sync::atomic::Ordering::Release);
        })),
        // After the fence that decides call 1, before its reply is written.
        fence_handled: Some(std::sync::Arc::new(move |_| {
            if finished.swap(false, std::sync::atomic::Ordering::AcqRel) {
                let _ = at_write_tx.send(());
                let _ = closed_rx.lock().unwrap().recv();
            }
        })),
        ..Default::default()
    };
    let (mut client, reader, session) = hooked_session(&identity, &registry, 90, None, hooks);
    writeln!(client, "{}", describe_call(1)).unwrap();
    at_write
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("call 1 was never decided");
    drop(reader);
    drop(client);
    closed_tx.send(()).unwrap();
    let ended = tokio::task::spawn_blocking(move || session.join().unwrap())
        .await
        .unwrap();
    assert!(
        ended.is_ok(),
        "a client hang-up failed the session: {ended:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn mcp_find_selected_snapshot_ranks_and_keeps_zero_hit_scope() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let mut source =
        "function Alpha() {}\nfunction Alphabet() {}\nfunction xAlpha() {}\nfunction Cafe\u{301}() {}\nfunction Café() {}\nfunction Écho() {}\nfunction écho() {}\nfunction $Find() {}\n".to_owned();
    for index in 0..51 {
        if index < 21 {
            source.push_str(&format!("function ManyDefault{index:03}() {{}}\n"));
        } else {
            source.push_str(&format!("function Many{index:03}() {{}}\n"));
        }
    }
    fs::write(root.join("a.js"), source).unwrap();
    fs::write(root.join("b.py"), "def separate(): pass\n").unwrap();
    fs::write(root.join("c.rs"), "fn unrelated() {}\n").unwrap();
    fs::write(root.join("d.java"), "class Unrelated {}\n").unwrap();
    fs::write(root.join("e.py"), "def another(): pass\n").unwrap();
    fs::write(root.join("f.java"), "class Another {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    let baseline = store.index_baseline().unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(&graph, &capture, &native, &writer, baseline, &cancel)
        .unwrap();
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let current = store
        .publish_native(&graph, &capture, &native, &writer, pin, &cancel)
        .unwrap();
    assert!(current.index_revision > pin.index_revision);
    store
        .mcp_set_coverage_for_tests(&writer, "b.py", true, "failed")
        .unwrap();
    store
        .mcp_set_coverage_for_tests(&writer, "d.java", false, "unsupported")
        .unwrap();
    store
        .mcp_set_coverage_for_tests(&writer, "e.py", true, "partial")
        .unwrap();
    store
        .mcp_set_coverage_for_tests(&writer, "f.java", false, "omitted")
        .unwrap();
    drop(writer);
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(143, &identity).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = mpsc::sync_channel(1);
    let resume_rx = std::sync::Mutex::new(resume_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        resume_rx
            .lock()
            .unwrap()
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    }));
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if entered_rx.try_recv().is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("startup reconciliation did not pause");
    assert!(runtime.catching_up());
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity),
            server,
            registry,
            143,
            handle,
        )
    });
    let ask = |client: &mut UnixStream,
               id: i64,
               query: &str,
               limit: Option<u64>,
               expected: Option<Value>| {
        let mut args = json!({"schemaVersion":1,"query":query});
        if let Some(limit) = limit {
            args["limit"] = json!(limit);
        }
        if let Some(pin) = expected {
            args["expectedBasis"] = pin;
        }
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_find_symbols","arguments":args})
            )
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str::<Value>(&line).unwrap()
    };
    let first = ask(&mut client, 144, "alpha", Some(1), None);
    tool(&first, None, true);
    let data = &first["result"]["structuredContent"];
    assert_eq!(data["catchingUp"], true);
    assert_eq!(
        data["evidenceBasis"],
        json!({
            "indexGeneration":current.index_generation.to_string(),"indexRevision":current.index_revision
        })
    );
    let state = runtime.state_version();
    resume_tx.send(()).unwrap();
    assert_eq!(data["data"]["symbols"][0]["name"], "Alpha");
    assert_eq!(data["truncated"], true);
    assert_eq!(data["truncationReason"], "limit");
    assert_eq!(data["data"]["provenance"].as_array().unwrap().len(), 1);
    // The hook runs after the Reconciling notification. The next state change
    // after release is the startup Ready commit or a recorded startup error.
    // catching_up may remain advisory-true for pending watcher work after Ready.
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    while runtime.catching_up() && runtime.state_version() == state {
        assert!(
            runtime.reconciliation_error().is_none(),
            "startup reconciliation failed: {:?}",
            runtime.reconciliation_error()
        );
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(
            !remaining.is_zero(),
            "startup reconciliation did not commit Ready: {:?}",
            runtime.reconciliation_error()
        );
        runtime.wait_state_change(state, remaining);
    }
    assert_ne!(
        runtime.state_version(),
        state,
        "startup Ready transition was not observed"
    );
    assert!(
        runtime.reconciliation_error().is_none(),
        "startup reconciliation failed: {:?}",
        runtime.reconciliation_error()
    );
    let ranked = ask(&mut client, 147, "ALPHA", Some(20), None);
    tool(&ranked, None, true);
    let ranked = &ranked["result"]["structuredContent"];
    assert_eq!(
        ranked["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["Alpha", "Alphabet", "xAlpha"]
    );
    assert_eq!(ranked["truncated"], false);
    let exact_limit = ask(&mut client, 149, "alpha", Some(3), None);
    tool(&exact_limit, None, true);
    assert_eq!(
        exact_limit["result"]["structuredContent"]["truncated"],
        false
    );
    let max_limit = ask(&mut client, 150, "Many", Some(50), None);
    tool(&max_limit, None, true);
    let max_symbols = max_limit["result"]["structuredContent"]["data"]["symbols"]
        .as_array()
        .unwrap()
        .len();
    assert!((1..=50).contains(&max_symbols));
    assert_eq!(
        max_limit["result"]["structuredContent"]["truncationReason"],
        if max_symbols == 50 {
            "limit"
        } else {
            "response_bytes"
        }
    );
    let default = ask(&mut client, 151, "ManyDefault", None, None);
    tool(&default, None, true);
    let default_data = &default["result"]["structuredContent"];
    let default_symbols = default_data["data"]["symbols"].as_array().unwrap();
    assert_eq!(default_symbols.len(), 20);
    assert_eq!(default_symbols[0]["name"], "ManyDefault000");
    assert_eq!(default_symbols[19]["name"], "ManyDefault019");
    assert_eq!(default_data["truncated"], true);
    assert_eq!(default_data["truncationReason"], "limit");
    let shorter = ask(&mut client, 156, "ManyDefault", Some(3), None);
    tool(&shorter, None, true);
    let shorter_data = &shorter["result"]["structuredContent"];
    assert_eq!(shorter_data["data"]["symbols"].as_array().unwrap().len(), 3);
    assert_eq!(shorter_data["truncated"], true);
    assert_eq!(shorter_data["truncationReason"], "limit");
    let unicode = ask(&mut client, 153, "Caf", Some(20), None);
    tool(&unicode, None, true);
    assert_eq!(
        unicode["result"]["structuredContent"]["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .take(2)
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Cafe\u{301}", "Café"]
    );
    let composed = ask(&mut client, 154, "Café", Some(20), None);
    tool(&composed, None, true);
    assert_eq!(
        composed["result"]["structuredContent"]["data"]["symbols"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for (query, names) in [
        ("ÉCHO", vec!["Écho"]),
        ("éCHO", vec!["écho"]),
        ("$Find", vec!["$Find"]),
        ("%", vec![]),
        ("*", vec![]),
        (".", vec![]),
    ] {
        let response = ask(&mut client, 157, query, Some(20), None);
        tool(&response, None, true);
        let actual: Vec<_> = response["result"]["structuredContent"]["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect();
        assert_eq!(actual, names, "literal ASCII-only match for {query:?}");
    }
    let id = ranked["data"]["symbols"][0]["symbolId"].as_str().unwrap();
    let id_only = ask(&mut client, 155, id, Some(20), None);
    tool(&id_only, None, true);
    assert_eq!(
        id_only["result"]["structuredContent"]["data"]["symbols"][0]["symbolId"],
        id
    );
    let literal = ask(&mut client, 148, " alpha ", Some(20), None);
    tool(&literal, None, true);
    assert_eq!(
        literal["result"]["structuredContent"]["data"]["symbols"],
        json!([])
    );
    let empty = ask(&mut client, 145, "no-such-name", Some(20), None);
    tool(&empty, None, true);
    let data = &empty["result"]["structuredContent"];
    assert_eq!(data["data"]["symbols"], json!([]));
    let coverage = data["data"]["coverage"].as_array().unwrap();
    assert_eq!(coverage.len(), 6);
    assert_eq!(
        coverage
            .iter()
            .map(|row| row["language"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["java", "java", "rust", "python", "python", "javascript"]
    );
    assert_eq!(
        coverage
            .iter()
            .map(|row| row["state"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "unsupported",
            "omitted",
            "complete",
            "failed",
            "partial",
            "complete"
        ]
    );
    assert_eq!(coverage[0]["selected"], false);
    assert_eq!(coverage[1]["selected"], false);
    assert_eq!(coverage[3]["selected"], true);
    assert_eq!(coverage[4]["selected"], true);
    assert_eq!(data["data"]["provenance"], json!([]));
    assert_eq!(data["partial"], true);
    assert_eq!(data["warnings"][0]["code"], "coverageIncomplete");
    assert_eq!(data["warnings"][1]["code"], "syntaxOnly");
    let old = json!({"indexGeneration":pin.index_generation.to_string(),"indexRevision":0});
    let stale = ask(&mut client, 146, "no-such-name", Some(20), Some(old));
    tool(&stale, Some("revision_conflict"), true);
    let retained = ask(
        &mut client,
        152,
        "not-a-declaration",
        Some(20),
        Some(
            json!({"indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision}),
        ),
    );
    tool(&retained, Some("revision_conflict"), true);
    let admitted = &retained["result"]["structuredContent"]["error"]["currentBasis"];
    assert_eq!(
        admitted["indexGeneration"],
        current.index_generation.to_string()
    );
    assert!(admitted["indexRevision"].as_u64().unwrap() >= current.index_revision);
    drop(client);
    session.join().unwrap().unwrap();
}

#[test]
fn mcp_find_cold_prior_head_uses_registry_roots_without_creating_index() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let index = topology.index_db(&identity);
    let mut registry = CheckoutRegistry::with_roots(topology.clone());
    registry.attach_launch(287, &identity).unwrap();
    assert!(
        registry
            .eligible_prior_head(&identity.root_key)
            .unwrap_err()
            .to_string()
            .contains("index_not_ready")
    );
    assert!(!index.exists(), "cold read created a derived database");
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    drop(store);
    registry.eligible_prior_head(&identity.root_key).unwrap();
    fs::write(&index, vec![b'X'; 4096]).unwrap();
    let invalid = registry
        .eligible_prior_head(&identity.root_key)
        .unwrap_err();
    assert!(
        invalid.to_string().contains("index_not_ready"),
        "a large non-index must not qualify as a published head: {invalid:#}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_find_required_rows_and_coverage_fail_closed() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    for fault in [
        "invalid_coverage",
        "missing_scope",
        "missing_header",
        "bad_digest",
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
        let store = Store::open_for_tests(temp.path(), &root).unwrap();
        store.mcp_prepare_virgin_queue_for_tests().unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &trellis::indexer::IndexOptions::new(root.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let writer = store.publication_for_tests().unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                &writer,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        drop(writer);
        let response = store.evidence_response().unwrap();
        let valid = response
            .mcp_find("Alpha", 20, None)
            .unwrap_or_else(|error| panic!("fixture find: {error}"));
        assert!(
            !valid.symbols.is_empty(),
            "fixture must prove a selected hit"
        );
        response.finish(()).unwrap();
        drop(response);
        let writer = store.publication_for_tests().unwrap();
        if fault == "invalid_coverage" {
            store
                .mcp_set_coverage_for_tests(&writer, "a.js", false, "failed")
                .unwrap();
        } else {
            store.mcp_corrupt_find_for_tests(&writer, fault).unwrap();
        }
        drop(writer);
        let response = store.evidence_response().unwrap();
        assert!(
            response.mcp_find("Alpha", 20, None).is_err(),
            "{fault} must fail as a whole without partial find evidence"
        );
        drop(response);
        drop(store);
        if fault == "invalid_coverage" {
            let identity = WorkspaceIdentity::discover(Some(&root), &root)
                .unwrap()
                .attach_marker()
                .unwrap();
            let topology = TopologyRoots::isolated_for_tests(
                temp.path().join("cache"),
                temp.path().join("data"),
            );
            let mut registry = CheckoutRegistry::with_roots(topology);
            registry.attach_launch(292, &identity).unwrap();
            let (entered_tx, entered_rx) = mpsc::sync_channel(1);
            let (release_tx, release_rx) = mpsc::sync_channel(1);
            let release_rx = std::sync::Mutex::new(release_rx);
            registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(trellis::testing::HANG_BACKSTOP)
                    .unwrap();
            }));
            let _runtime = registry.activate(&identity.root_key).unwrap();
            entered_rx
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap();
            let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
            let (mut client, mut reader, session) = socket_session(&identity, &registry, 292, None);
            writeln!(client, "{}", modern(json!(1), "tools/call", json!({
                "name":"trellis_find_symbols","arguments":{"schemaVersion":1,"query":"Alpha"}
            }))).unwrap();
            let reply = next_reply(&mut reader);
            tool(&reply, Some("store_unavailable"), true);
            assert!(reply["result"]["structuredContent"]["data"].is_null());
            release_tx.send(()).unwrap();
            drop(reader);
            drop(client);
            session.join().unwrap().unwrap();
        }
    }
}

fn linked_find_registry(
    temp: &Path,
    session: u64,
) -> (
    trellis::store::topology::WorkspaceIdentity,
    PathBuf,
    std::sync::Arc<tokio::sync::Mutex<trellis::daemon::registry::CheckoutRegistry>>,
    std::sync::Arc<trellis::daemon::registry::CheckoutRuntime>,
) {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let (main, linked) = git_linked_fixture(temp);
    fs::write(linked.join("a.js"), "function held() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&main), &main)
        .unwrap()
        .attach_marker()
        .unwrap();
    let selected = WorkspaceIdentity::discover(Some(&linked), &linked)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp, &linked).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(linked.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    drop(store);
    let topology = TopologyRoots::isolated_for_tests(temp.join("cache"), temp.join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(session, &identity).unwrap();
    registry.select(session, &identity, &linked).unwrap();
    let runtime = registry.activate(&selected.root_key).unwrap();
    (
        identity,
        linked,
        std::sync::Arc::new(tokio::sync::Mutex::new(registry)),
        runtime,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_find_linked_selected_success_cancel_and_final_root() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, linked, registry, runtime) = linked_find_registry(temp.path(), 291);
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 291, None);
    let selected_call = |id: i64| {
        modern(
            json!(id),
            "tools/call",
            json!({
                "name":"trellis_find_symbols",
                "arguments":{"schemaVersion":1,"workspace":linked,"query":"held"}
            }),
        )
    };
    writeln!(client, "{}", selected_call(1)).unwrap();
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["workspace"], json!(linked), "{reply}");
    assert!(envelope["catchingUp"].is_boolean());
    assert_eq!(envelope["data"]["symbols"][0]["name"], "held");
    assert_eq!(envelope["data"]["provenance"].as_array().unwrap().len(), 1);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();

    let cancel_gate = ReadGate::default();
    runtime.set_mcp_admitted_gate_for_tests(cancel_gate.hook());
    let (read_tx, read_rx) = mpsc::sync_channel(1);
    let frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let frame_count = frames.clone();
    let hooks = trellis::mcp::TransportHooksForTests {
        frame_read: Some(std::sync::Arc::new(move || {
            if frame_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                let _ = read_tx.try_send(());
            }
        })),
        ..Default::default()
    };
    let (mut client, mut reader, session) = hooked_session(&identity, &registry, 291, None, hooks);
    writeln!(client, "{}", selected_call(4)).unwrap();
    let waiter = cancel_gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    writeln!(
        client,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4}})
    )
    .unwrap();
    read_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    cancel_gate.open();
    writeln!(client, "{}", modern(json!(5), "tools/list", json!({}))).unwrap();
    let reply = next_reply(&mut reader);
    assert_eq!(
        reply["id"], 5,
        "cancelled W2 selected call emitted evidence: {reply}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();

    let final_gate = ReadGate::default();
    runtime.set_mcp_admitted_gate_for_tests(final_gate.hook());
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 291, None);
    writeln!(client, "{}", selected_call(3)).unwrap();
    let waiter = final_gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    fs::rename(&linked, temp.path().join("moved-linked")).unwrap();
    final_gate.open();
    let reply = next_reply(&mut reader);
    tool(&reply, Some("root_changed"), true);
    assert_eq!(
        reply["result"]["structuredContent"]["workspace"],
        json!(linked),
        "{reply}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[test]
fn mcp_find_null_name_and_id_byte_ties_are_ranked() {
    use trellis::store::Store;
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
    fs::write(root.join("b.js"), "function Beta() {}\n").unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    let response = store.evidence_response().unwrap();
    let result = response
        .mcp_find("sid:v1:", 50, None)
        .unwrap_or_else(|error| panic!("ID find: {error:#}"));
    assert!(result.symbols.len() >= 4);
    assert!(
        result.symbols[0].declaration.name.is_none(),
        "null name precedes named ID matches"
    );
    let nulls: Vec<_> = result
        .symbols
        .iter()
        .take_while(|symbol| symbol.declaration.name.is_none())
        .map(|symbol| symbol.declaration.syntax_id.as_str())
        .collect();
    assert!(nulls.len() >= 2, "one unnamed module per source document");
    assert!(
        nulls
            .windows(2)
            .all(|ids| ids[0].as_bytes() <= ids[1].as_bytes())
    );
    assert!(
        result
            .symbols
            .iter()
            .skip(nulls.len())
            .any(|symbol| symbol.declaration.name.is_some())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_find_cold_coordinator_admits_prior_head_only_under_configured_roots() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let index = topology.index_db(&identity);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(293, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 293, None);
    let call = |id: i64| {
        modern(
            json!(id),
            "tools/call",
            json!({
                "name":"trellis_find_symbols","arguments":{"schemaVersion":1,"query":"Alpha"}
            }),
        )
    };
    writeln!(client, "{}", call(1)).unwrap();
    tool(&next_reply(&mut reader), Some("index_not_ready"), true);
    assert!(
        !index.exists(),
        "cold W2 created derived state without a prior head"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();

    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    drop(store);
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    {
        let mut registry = registry.lock().await;
        registry.attach_launch(294, &identity).unwrap();
        assert!(registry.runtime(&identity.root_key).is_none());
        registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap();
        }));
    }
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 294, None);
    writeln!(client, "{}", call(2)).unwrap();
    entered_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["catchingUp"], true);
    assert_eq!(envelope["data"]["symbols"][0]["name"], "Alpha");
    assert_eq!(
        envelope["evidenceBasis"],
        json!({
            "indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision
        })
    );
    release_tx.send(()).unwrap();
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_find_linked_selected_deadline_after_admission_keeps_resolved_shape() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, linked, registry, runtime) = linked_find_registry(temp.path(), 295);
    let gate = ReadGate::default();
    runtime.set_mcp_admitted_gate_for_tests(gate.hook());
    // Only W2 consumes this injected contention. The call cannot finish while
    // the post-admission gate holds its selected read.
    runtime.mcp_busy_reads_for_tests(1).unwrap();
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 295, None);
    writeln!(
        client,
        "{}",
        modern(
            json!(1),
            "tools/call",
            json!({
                "name":"trellis_find_symbols",
                "arguments":{"schemaVersion":1,"workspace":linked,"query":"held"}
            })
        )
    )
    .unwrap();
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    // Admission was observed before the test clock advances. One retryable
    // read now returns to the worker loop, where the virtual deadline wins.
    runtime.advance_mcp_clock_for_tests(trellis::mcp::CALL_DEADLINE + Duration::from_secs(1));
    gate.open();
    let reply = next_reply(&mut reader);
    tool(&reply, Some("deadline_exceeded"), true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["workspace"], json!(linked), "{reply}");
    assert!(envelope["catchingUp"].is_boolean(), "{reply}");
    assert!(
        envelope["error"].get("attemptedWorkspace").is_none(),
        "{reply}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_declaration_selected_identity_and_measured_record() {
    use std::os::unix::net::UnixStream;
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(
        root.join("a.js"),
        "function Alpha(first, second) { return first; }\n",
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(143, &identity).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    }));
    registry.activate(&identity.root_key).unwrap();
    entered_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity),
            server,
            registry,
            143,
            handle,
        )
    });
    let ask = |client: &mut UnixStream, id: i64, name: &str, args: Value| -> Value {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":name,"arguments":args})
            )
        )
        .unwrap();
        let mut line = String::new();
        BufReader::new(client.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str(&line).unwrap()
    };
    let found = ask(
        &mut client,
        1,
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"Alpha"}),
    );
    tool(&found, None, true);
    let symbol = &found["result"]["structuredContent"]["data"]["symbols"][0];
    let id = symbol["symbolId"].as_str().unwrap();
    let inspected = ask(
        &mut client,
        2,
        "trellis_inspect",
        json!({"schemaVersion":1,"symbolId":id,"view":"declaration"}),
    );
    tool(&inspected, None, true);
    let envelope = &inspected["result"]["structuredContent"];
    assert_eq!(
        envelope["catchingUp"], true,
        "validated prior head during H"
    );
    release_tx.send(()).unwrap();
    assert_eq!(envelope["data"]["declaration"]["syntaxId"], id);
    assert_eq!(envelope["data"]["displayKey"], "a.js#Alpha");
    assert_eq!(envelope["data"]["contentHash"], symbol["contentHash"]);
    assert_eq!(envelope["data"]["coverage"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["data"]["provenance"].as_array().unwrap().len(), 1);
    assert_eq!(
        envelope["data"]["declaration"]["header"]["parameters"][0]["name"],
        "first"
    );
    assert_eq!(
        envelope["data"]["declaration"]["header"]["parameters"][1]["name"],
        "second"
    );
    let found = ask(
        &mut client,
        3,
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"sid:v1:"}),
    );
    tool(&found, None, true);
    let unnamed = found["result"]["structuredContent"]["data"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"].is_null() && s["document"]["path"] == "a.js")
        .expect("selected unnamed module");
    let inspected = ask(
        &mut client,
        4,
        "trellis_inspect",
        json!({
            "schemaVersion":1,"view":"declaration","symbolId":unnamed["symbolId"]
        }),
    );
    tool(&inspected, None, true);
    assert_eq!(
        inspected["result"]["structuredContent"]["data"]["displayKey"],
        format!(
            "a.js#{}@{}",
            unnamed["kind"].as_str().unwrap(),
            unnamed["range"]["start"].as_u64().unwrap()
        )
    );
    drop(client);
    session.join().unwrap().unwrap();
}

#[test]
fn mcp_declaration_body_edit_retains_id_but_changes_selected_hash_and_pin() {
    use trellis::store::Store;
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let path = root.join("a.js");
    fs::write(&path, "function Alpha(first, second) { return first; }\n").unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = store.publication_for_tests().unwrap();
    let publish = |expected| {
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &trellis::indexer::IndexOptions::new(root.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        store
            .publish_native(&graph, &capture, &native, &writer, expected, &cancel)
            .unwrap()
    };
    let first = publish(store.index_baseline().unwrap());
    let selected = store.evidence_response().unwrap();
    let found = selected.mcp_find("Alpha", 20, None).unwrap();
    let symbol_id = found.symbols[0].declaration.syntax_id.clone();
    let original = selected.mcp_declaration(&symbol_id, None).unwrap().unwrap();
    assert_eq!(original.declaration.header.parameters.len(), 2);
    assert_eq!(
        original.declaration.header.parameters[0].name.as_deref(),
        Some("first")
    );
    assert_eq!(
        original.declaration.header.parameters[1].name.as_deref(),
        Some("second")
    );
    assert_eq!(
        original.provenance[0].id,
        original.declaration.provenance_id
    );
    assert_eq!(original.provenance[0].content_hash, original.content_hash);
    assert_eq!(original.coverage[0].document_path, "a.js");
    let module = selected
        .mcp_find("sid:v1:", 50, None)
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| s.declaration.name.is_none())
        .unwrap();
    assert!(
        selected
            .mcp_declaration(&module.declaration.syntax_id, None)
            .unwrap()
            .unwrap()
            .declaration
            .name
            .is_none()
    );
    drop(selected);
    fs::write(&path, "function Alpha(first, second) { return second; }\n").unwrap();
    let second = publish(first);
    assert_ne!(first, second);
    let selected = store.evidence_response().unwrap();
    let stale = selected
        .mcp_declaration(&format!("sid:v1:{}", "0".repeat(32)), Some(first))
        .err()
        .unwrap();
    assert!(
        stale.to_string().starts_with("revision conflict:"),
        "{stale:#}"
    );
    let changed = selected.mcp_declaration(&symbol_id, None).unwrap().unwrap();
    assert_eq!(changed.declaration.syntax_id, symbol_id);
    assert_ne!(changed.content_hash, original.content_hash);
    assert_ne!(
        changed.provenance[0].revision_id,
        original.provenance[0].revision_id
    );
    assert_eq!(changed.provenance[0].content_hash, changed.content_hash);
    assert!(
        selected
            .mcp_declaration(&format!("sid:v1:{}", "0".repeat(32)), None)
            .unwrap()
            .is_none()
    );
}

#[test]
fn mcp_declaration_corrupt_required_proof_refuses_whole_result() {
    use trellis::store::Store;
    for fault in ["missing_header", "bad_digest"] {
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
        let store = Store::open_for_tests(temp.path(), &root).unwrap();
        store.mcp_prepare_virgin_queue_for_tests().unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &trellis::indexer::IndexOptions::new(root.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let writer = store.publication_for_tests().unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                &writer,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        drop(writer);
        let response = store.evidence_response().unwrap();
        let id = response.mcp_find("Alpha", 20, None).unwrap().symbols[0]
            .declaration
            .syntax_id
            .clone();
        assert!(response.mcp_declaration(&id, None).unwrap().is_some());
        response.finish(()).unwrap();
        drop(response);
        let writer = store.publication_for_tests().unwrap();
        store.mcp_corrupt_find_for_tests(&writer, fault).unwrap();
        drop(writer);
        let response = store.evidence_response().unwrap();
        let error = response.mcp_declaration(&id, None).err().unwrap();
        assert!(
            !error.to_string().starts_with("revision conflict:"),
            "{fault}: {error:#}"
        );
    }
}

#[test]
fn mcp_declaration_rejects_noncanonical_id_before_read() {
    use trellis::mcp::tools::validate;
    for invalid in [
        "sid:v1:ABCDEF0123456789abcdef0123456789",
        "sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "sid:v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaag",
    ] {
        assert_eq!(
            validate(
                "trellis_inspect",
                Some(&json!({
                    "schemaVersion":1,"view":"declaration","symbolId":invalid
                }))
            ),
            Err("invalid_request")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_declaration_refuses_oversized_whole_wire_dto() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let name = format!("Alpha{}", "a".repeat(12_000));
    fs::write(root.join("a.js"), format!("function {name}() {{}}\n")).unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    let id = store
        .evidence_response()
        .unwrap()
        .mcp_find("Alpha", 1, None)
        .unwrap()
        .symbols[0]
        .declaration
        .syntax_id
        .clone();
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(143, &identity).unwrap();
    registry.activate(&identity.root_key).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 143, None);
    writeln!(client, "{}", modern(json!(1), "tools/call", json!({
        "name":"trellis_inspect", "arguments":{"schemaVersion":1,"view":"declaration","symbolId":id}
    }))).unwrap();
    let response = next_reply(&mut reader);
    tool(&response, Some("range_too_large"), true);
    assert!(response["result"]["structuredContent"]["data"].is_null());
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_declaration_cold_head_precedes_complete_pin_conflict() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let index = topology.index_db(&identity);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(143, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 143, None);
    writeln!(client, "{}", modern(json!(1), "tools/call", json!({
        "name":"trellis_inspect", "arguments":{"schemaVersion":1,"view":"declaration",
        "symbolId":format!("sid:v1:{}", "0".repeat(32)),
        "expectedBasis":{"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":1}}
    }))).unwrap();
    let response = next_reply(&mut reader);
    tool(&response, Some("index_not_ready"), true);
    assert!(!index.exists(), "cold inspect created a derived database");
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_selected_direct_calls_and_unresolved_hints() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(
        root.join("a.js"),
        "function Alpha() { Beta(); }\nfunction Beta() {}\n",
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(300, &identity).unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
        entered_tx.send(()).unwrap();
        release_rx
            .lock()
            .unwrap()
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
    }));
    registry.activate(&identity.root_key).unwrap();
    entered_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 300, None);
    let ask = |client: &mut std::os::unix::net::UnixStream,
               reader: &mut BufReader<std::os::unix::net::UnixStream>,
               id,
               name,
               args: Value| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":name,"arguments":args})
            )
        )
        .unwrap();
        next_reply(reader)
    };
    let found = ask(
        &mut client,
        &mut reader,
        1,
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"Alpha"}),
    );
    tool(&found, None, true);
    let symbol_id = &found["result"]["structuredContent"]["data"]["symbols"][0]["symbolId"];
    let response = ask(
        &mut client,
        &mut reader,
        2,
        "trellis_inspect",
        json!({"schemaVersion":1,"symbolId":symbol_id,"view":"outgoing_calls"}),
    );
    tool(&response, None, true);
    let data = &response["result"]["structuredContent"]["data"];
    assert_eq!(response["result"]["structuredContent"]["catchingUp"], true);
    assert_eq!(data["calls"].as_array().unwrap().len(), 1);
    assert_eq!(data["calls"][0]["calleeText"], "Beta");
    assert_eq!(data["calls"][0]["disposition"], "unresolved");
    assert!(data["calls"][0]["binding"].is_null());
    let mut stale = found["result"]["structuredContent"]["evidenceBasis"].clone();
    stale["indexRevision"] = json!(stale["indexRevision"].as_u64().unwrap() + 1);
    let refused = ask(
        &mut client,
        &mut reader,
        3,
        "trellis_inspect",
        json!({"schemaVersion":1,"symbolId":format!("sid:v1:{}","0".repeat(32)),
            "view":"outgoing_calls","expectedBasis":stale}),
    );
    tool(&refused, Some("revision_conflict"), true);
    assert_eq!(
        refused["result"]["structuredContent"]["error"]["currentBasis"],
        found["result"]["structuredContent"]["evidenceBasis"]
    );
    release_tx.send(()).unwrap();
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

fn mcp_outgoing_fixture(files: &[(&str, &str)], owner: &str, limit: Option<usize>) -> Value {
    mcp_outgoing_fixture_with_budget(files, owner, limit, None)
}

fn mcp_outgoing_fixture_with_budget(
    files: &[(&str, &str)],
    owner: &str,
    limit: Option<usize>,
    advance_hints: Option<Vec<(&'static str, u64)>>,
) -> Value {
    mcp_outgoing_wire_fixture(files, owner, limit, advance_hints, false).0
}

fn mcp_outgoing_wire_fixture(
    files: &[(&str, &str)],
    owner: &str,
    limit: Option<usize>,
    advance_hints: Option<Vec<(&'static str, u64)>>,
    legacy: bool,
) -> (Value, Vec<u8>) {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    for (path, text) in files {
        fs::write(root.join(path), text).unwrap();
    }
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(301, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 301, None);
    if legacy {
        writeln!(
            client,
            "{}",
            request(
                json!(0),
                "initialize",
                fixture()["legacyInitialize"].clone()
            )
        )
        .unwrap();
        assert_eq!(
            next_reply(&mut reader)["result"]["protocolVersion"],
            "2025-11-25"
        );
        writeln!(
            client,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
    }
    writeln!(
        client,
        "{}",
        call(
            legacy,
            json!(1),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":owner})
        )
    )
    .unwrap();
    let found = next_reply(&mut reader);
    tool(&found, None, !legacy);
    let symbols = found["result"]["structuredContent"]["data"]["symbols"]
        .as_array()
        .unwrap();
    let symbol = symbols.iter().find(|s| s["name"] == owner).unwrap();
    let mut args = json!({"schemaVersion":1,"symbolId":symbol["symbolId"],"view":"outgoing_calls"});
    if let Some(limit) = limit {
        args["limit"] = json!(limit);
    }
    let budget_gate = if let Some(stages) = advance_hints {
        let path = temp.path().join("hint.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(false).unwrap();
        unsafe {
            std::env::set_var("TRELLIS_TEST_MCP_PHASE", stages[0].0);
            std::env::set_var("TRELLIS_TEST_MCP_PHASE_SOCKET", &path);
        }
        Some(std::thread::spawn(move || {
            for (index, (stage, millis)) in stages.iter().enumerate() {
                let (mut gate, _) = listener.accept().unwrap();
                gate.set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
                    .unwrap();
                let mut received = String::new();
                BufReader::new(gate.try_clone().unwrap())
                    .read_line(&mut received)
                    .unwrap();
                assert_eq!(received, format!("{stage}\n"));
                runtime.advance_mcp_clock_for_tests(Duration::from_millis(*millis));
                if let Some((next_stage, _)) = stages.get(index + 1) {
                    unsafe { std::env::set_var("TRELLIS_TEST_MCP_PHASE", next_stage) };
                }
                gate.write_all(b"!").unwrap();
            }
        }))
    } else {
        None
    };
    writeln!(
        client,
        "{}",
        call(legacy, json!(2), "trellis_inspect", args)
    )
    .unwrap();
    let mut frame = Vec::new();
    reader.read_until(b'\n', &mut frame).unwrap();
    assert_eq!(
        frame.last(),
        Some(&b'\n'),
        "response must be a complete frame"
    );
    let response: Value = serde_json::from_slice(&frame).unwrap();
    if let Some(gate) = budget_gate {
        gate.join().unwrap();
        unsafe {
            std::env::remove_var("TRELLIS_TEST_MCP_PHASE");
            std::env::remove_var("TRELLIS_TEST_MCP_PHASE_SOCKET");
        }
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    (response, frame)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_nested_owner_and_compound_member_exclusion() {
    let reply = mcp_outgoing_fixture(
        &[(
            "a.js",
            "function Target() {}\nfunction Outer() { Target(); function Inner() { Target(); } obj.Target(); }\n",
        )],
        "Outer",
        None,
    );
    tool(&reply, None, true);
    let data = &reply["result"]["structuredContent"]["data"];
    let calls = data["calls"].as_array().unwrap();
    assert_eq!(
        calls.len(),
        2,
        "nested Inner call must not become Outer call: {reply}"
    );
    assert_eq!(calls[0]["calleeText"], "Target");
    assert_eq!(calls[1]["calleeText"], "Target");
    assert_eq!(
        calls[0]["nativeCandidates"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        calls[1]["nativeCandidates"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for call in calls {
        assert_eq!(call["disposition"], "unresolved");
        assert!(call["targetId"].is_null());
        assert!(call["binding"].is_null());
    }
    assert!(
        calls[0]["range"]["start"].as_u64().unwrap() < calls[1]["range"]["start"].as_u64().unwrap()
    );
    let inner = mcp_outgoing_fixture(
        &[(
            "a.js",
            "function Target() {}\nfunction Outer() { function Inner() { Target(); } }\n",
        )],
        "Inner",
        None,
    );
    tool(&inner, None, true);
    assert_eq!(
        inner["result"]["structuredContent"]["data"]["calls"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_nine_hints_exact_omissions_and_language_scope() {
    let mut files: Vec<(String, String)> = (0..9)
        .map(|i| (format!("b{i}.js"), "function Target() {}\n".to_owned()))
        .collect();
    files.push(("a.js".into(), "function Owner() { Target(); }\n".into()));
    files.push(("c.py".into(), "def Target():\n    pass\n".into()));
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();
    let reply = mcp_outgoing_fixture(&borrowed, "Owner", None);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    let data = &envelope["data"];
    let call = &data["calls"][0];
    let hints = call["nativeCandidates"]["items"].as_array().unwrap();
    assert_eq!(hints.len(), 8, "{reply}");
    assert_eq!(call["nativeCandidates"]["omitted"], 1);
    assert_eq!(call["nativeCandidates"]["complete"], false);
    assert_eq!(envelope["truncationReason"], "limit");
    assert_eq!(data["coverage"].as_array().unwrap().len(), 9);
    assert_eq!(data["provenance"].as_array().unwrap().len(), 9);
    assert_eq!(hints[0]["document"]["path"], "b0.js");
    assert_eq!(hints[7]["document"]["path"], "b7.js");
    assert!(
        hints
            .iter()
            .all(|candidate| candidate["document"]["language"] == "javascript")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_limit_truncation_preserves_call_first() {
    let reply = mcp_outgoing_fixture(
        &[(
            "a.js",
            "function Target() {}\nfunction Owner() { Target(); Target(); }\n",
        )],
        "Owner",
        Some(1),
    );
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["data"]["calls"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["truncationReason"], "limit");
    assert_eq!(
        envelope["data"]["calls"][0]["nativeCandidates"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(envelope["partial"], true);
    assert_eq!(envelope["data"]["calls"][0]["binding"], Value::Null);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_hooked_optional_budget_does_not_refuse_mandatory_calls() {
    let reply = mcp_outgoing_fixture_with_budget(
        &[(
            "a.js",
            "function Target() {}\nfunction Owner() { Target(); Target(); }\n",
        )],
        "Owner",
        None,
        Some(vec![("outgoing_hint", 1001)]),
    );
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["data"]["calls"].as_array().unwrap().len(), 2);
    for call in envelope["data"]["calls"].as_array().unwrap() {
        assert_eq!(call["nativeCandidates"]["items"], json!([]));
        assert!(call["nativeCandidates"]["omitted"].is_null());
        assert_eq!(call["nativeCandidates"]["complete"], false);
        assert_eq!(call["disposition"], "unresolved");
    }
    assert_eq!(
        envelope["truncated"], false,
        "unknown optional hints alone are not truncation"
    );
    assert_eq!(
        envelope["partial"], true,
        "unresolved boundary calls remain partial"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_compound_expression_has_no_guessed_hint() {
    let reply = mcp_outgoing_fixture(
        &[(
            "a.js",
            "function Target() {}\nfunction Owner() { (obj.Target)(); Target(); }\n",
        )],
        "Owner",
        None,
    );
    tool(&reply, None, true);
    let calls = reply["result"]["structuredContent"]["data"]["calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2);
    assert!(
        calls[0]["calleeRange"].is_null(),
        "compound callee cannot be a token: {reply}"
    );
    assert!(calls[0]["calleeText"].is_null());
    assert_eq!(calls[0]["nativeCandidates"]["items"], json!([]));
    assert_eq!(calls[0]["nativeCandidates"]["omitted"], 0);
    assert_eq!(
        calls[1]["nativeCandidates"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_response_bytes_call_prefix_before_global_hints() {
    let mut files: Vec<(String, String)> = (0..9)
        .map(|i| (format!("b{i}.js"), "function Target() {}\n".to_owned()))
        .collect();
    let mut source = "function Owner() {\n".to_owned();
    for _ in 0..50 {
        source.push_str(" Target();\n");
    }
    source.push_str("}\n");
    files.push(("a.js".into(), source));
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();
    let reply = mcp_outgoing_fixture(&borrowed, "Owner", Some(50));
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    let calls = envelope["data"]["calls"].as_array().unwrap();
    assert!(!calls.is_empty() && calls.len() < 50, "{reply}");
    assert_eq!(envelope["truncationReason"], "response_bytes");
    let mut optional_budget_expired = false;
    for (index, call) in calls.iter().enumerate() {
        assert_eq!(call["ordinal"], json!(index));
        assert_eq!(call["disposition"], "unresolved");
        let candidates = &call["nativeCandidates"];
        let count = candidates["items"].as_array().unwrap().len();
        assert_eq!(candidates["complete"], false);
        if candidates["omitted"].is_null() {
            // The optional progress budget expired before this call's hints completed.
            // Partial hint work is discarded, and every later call stays unknown.
            optional_budget_expired = true;
            assert_eq!(candidates["items"], json!([]), "{reply}");
        } else {
            assert!(
                !optional_budget_expired,
                "optional hint budget cannot resume"
            );
            assert_eq!(candidates["omitted"], json!(9 - count), "{reply}");
        }
        if index > 0 {
            assert!(
                count == 0
                    || calls[index - 1]["nativeCandidates"]["items"]
                        .as_array()
                        .unwrap()
                        .len()
                        == 8
            );
        }
    }
    let closure = &envelope["data"];
    let proofs: std::collections::BTreeSet<_> = closure["provenance"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    let required: std::collections::BTreeSet<_> = calls
        .iter()
        .flat_map(|call| {
            std::iter::once(call["provenanceId"].as_str().unwrap()).chain(
                call["nativeCandidates"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| c["provenanceId"].as_str().unwrap()),
            )
        })
        .collect();
    assert_eq!(proofs, required);
    assert!(serde_json::to_vec(&reply).unwrap().len() < 65_536);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_near_call_deadline_preserves_required_call_without_optional_work() {
    // Required calls finish before the hint budget starts. The row gate advances
    // the virtual clock into the reserved send-decision window, without crossing
    // the overall deadline. Only optional hints may be abandoned.
    let reply = mcp_outgoing_fixture_with_budget(
        &[(
            "a.js",
            "function Target() {}\nfunction Owner() { Target(); }\n",
        )],
        "Owner",
        None,
        Some(vec![
            ("outgoing_hint_start", 4000),
            ("outgoing_hint_row", 940),
        ]),
    );
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    let calls = envelope["data"]["calls"].as_array().unwrap();
    assert_eq!(
        calls.len(),
        1,
        "mandatory call must survive optional budget: {reply}"
    );
    assert_eq!(calls[0]["calleeText"], "Target");
    assert_eq!(calls[0]["disposition"], "unresolved");
    assert_eq!(
        calls[0]["nativeCandidates"]["items"],
        json!([]),
        "optional hints must not spend the remaining required call deadline: {reply}"
    );
    assert_eq!(
        calls[0]["nativeCandidates"]["omitted"],
        Value::Null,
        "optional hint count is unknown at the deadline: {reply}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_shared_hint_budget_keeps_a_and_marks_b_c_unknown() {
    // Gate exactly before B's budget check: the first call has finished matching.
    // Exhausting the shared budget cannot erase A or invent a zero for B/C.
    let reply = mcp_outgoing_fixture_with_budget(
        &[(
            "a.js",
            "function Target() {}\nfunction Owner() { Target(); Target(); Target(); }\n",
        )],
        "Owner",
        None,
        Some(vec![("outgoing_hint_item_1", 1001)]),
    );
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    let calls = envelope["data"]["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 3, "all required calls must survive: {reply}");
    for (i, call) in calls.iter().enumerate() {
        assert_eq!(call["ordinal"], json!(i));
        assert_eq!(call["calleeText"], "Target");
        assert_eq!(call["disposition"], "unresolved");
        assert_eq!(call["nativeCandidates"]["complete"], false);
    }
    assert_eq!(
        calls[0]["nativeCandidates"]["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(calls[0]["nativeCandidates"]["omitted"], 0);
    for call in &calls[1..] {
        assert_eq!(call["nativeCandidates"]["items"], json!([]));
        assert_eq!(call["nativeCandidates"]["omitted"], Value::Null);
    }
    assert_eq!(envelope["truncated"], false);
    assert_eq!(envelope["partial"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_complete_modern_legacy_frames_fill_candidates_after_all_calls() {
    let mut files: Vec<(String, String)> = (0..9)
        .map(|i| {
            (
                format!("candidate{i:02}.js"),
                "function Target() {}\n".to_owned(),
            )
        })
        .collect();
    let mut source = "function Owner() {\n".to_owned();
    for _ in 0..24 {
        source.push_str("  Target();\n");
    }
    source.push_str("}\n");
    files.push(("owner.js".into(), source));
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();
    for legacy in [false, true] {
        let (reply, frame) = mcp_outgoing_wire_fixture(&borrowed, "Owner", Some(50), None, legacy);
        tool(&reply, None, !legacy);
        assert_eq!(frame.last(), Some(&b'\n'));
        assert!(
            frame.len() <= 65_536,
            "complete dual-content frame exceeds cap: {}",
            frame.len()
        );
        assert_eq!(serde_json::from_slice::<Value>(&frame).unwrap(), reply);
        let envelope = &reply["result"]["structuredContent"];
        let calls = envelope["data"]["calls"].as_array().unwrap();
        assert_eq!(
            calls.len(),
            24,
            "all required calls must fit before hints: {legacy} {}",
            frame.len()
        );
        assert_eq!(
            envelope["truncationReason"],
            "response_bytes",
            "{legacy} {}",
            frame.len()
        );
        assert_eq!(envelope["truncated"], true);
        let mut first_omission = None;
        for (i, call) in calls.iter().enumerate() {
            assert_eq!(call["ordinal"], json!(i));
            let shown = call["nativeCandidates"]["items"].as_array().unwrap().len();
            assert!(shown <= 8);
            assert_eq!(call["nativeCandidates"]["omitted"], json!(9 - shown));
            assert_eq!(call["nativeCandidates"]["complete"], false);
            if shown < 8 {
                first_omission.get_or_insert(i);
            } else {
                assert!(
                    first_omission.is_none(),
                    "candidate prefix resumed after omission"
                );
            }
        }
        assert!(
            first_omission.is_some(),
            "candidate-only overflow was not reached"
        );
        let provenance: std::collections::BTreeSet<_> = envelope["data"]["provenance"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["id"].as_str().unwrap())
            .collect();
        let required: std::collections::BTreeSet<_> = calls
            .iter()
            .flat_map(|call| {
                std::iter::once(call["provenanceId"].as_str().unwrap()).chain(
                    call["nativeCandidates"]["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|item| item["provenanceId"].as_str().unwrap()),
                )
            })
            .collect();
        assert_eq!(
            provenance, required,
            "each emitted hint has proof; no excluded proof"
        );
    }
}

/// Mutate an already published derived fixture only while no Store/SQLite handle is live.
/// The next selected response must either omit an unverified optional hint or fail
/// the whole required answer, never quietly claim a complete proof.
fn mcp_outgoing_corrupt_boundary_fixture(fault: &str) -> Value {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("owner.js"), "function Owner() { Target(); }\n").unwrap();
    fs::write(root.join("target.js"), "function Target() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    let selected = store.evidence_response().unwrap();
    let owner_id = selected
        .mcp_find("Owner", 20, None)
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| s.declaration.name.as_deref() == Some("Owner"))
        .unwrap()
        .declaration
        .syntax_id;
    selected.finish(()).unwrap();
    drop(selected);
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let db_path = topology.index_db(&identity);
    {
        let db = rusqlite::Connection::open(&db_path).unwrap();
        db.pragma_update(None, "foreign_keys", "OFF").unwrap();
        // The ID rewrite also leaves a keyed header orphan. This tests
        // optional closure rejection, not an isolated canonical-ID branch.
        // Native provenance IDs are derived from the selected V8 scope; there
        // is no independent stored optional proof ID to corrupt here.
        let (sql, expect) = match fault {
            "orphaned_optional_id" => (
                "UPDATE native_version_declarations SET syntax_id='not-a-canonical-syntax-id' WHERE name='Target'",
                1,
            ),
            "missing_optional_header" => (
                "DELETE FROM native_version_headers WHERE syntax_id IN (SELECT syntax_id FROM native_version_declarations WHERE name='Target')",
                1,
            ),
            "invalid_optional_coverage" => (
                "UPDATE revision_documents SET coverage_selected=0,coverage_state='omitted',coverage_diagnostic='fixture unselected' WHERE path='target.js'",
                1,
            ),
            "foreign_source_set" => (
                "UPDATE revision_documents SET source_set_id='source-set:v1:foreign' WHERE path='target.js'",
                1,
            ),
            "missing_required_owner_header" => (
                "DELETE FROM native_version_headers WHERE syntax_id IN (SELECT syntax_id FROM native_version_declarations WHERE name='Owner')",
                1,
            ),
            "missing_required_owner_coverage" => (
                "UPDATE revision_documents SET coverage_selected=0,coverage_state='omitted',coverage_diagnostic='fixture unselected' WHERE path='owner.js'",
                1,
            ),
            "invalid_required_call_id" => (
                "UPDATE native_version_calls SET id='not-a-canonical-call-id' WHERE owner_syntax_id IN (SELECT syntax_id FROM native_version_declarations WHERE name='Owner')",
                1,
            ),
            _ => panic!("unknown fixture fault: {fault}"),
        };
        assert_eq!(db.execute(sql, []).unwrap(), expect, "{fault}");
    }
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(302, &identity).unwrap();
    registry.activate(&identity.root_key).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 302, None);
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(2),
            "trellis_inspect",
            json!({
                "schemaVersion":1,"symbolId":owner_id,"view":"outgoing_calls"
            })
        )
    )
    .unwrap();
    let reply = next_reply(&mut reader);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    reply
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_unverified_optional_candidate_never_poison_required_answer() {
    for fault in [
        "orphaned_optional_id",
        "missing_optional_header",
        "invalid_optional_coverage",
    ] {
        let reply = mcp_outgoing_corrupt_boundary_fixture(fault);
        assert!(
            reply["result"]["structuredContent"]["error"].is_null(),
            "{fault}: {reply}"
        );
        tool(&reply, None, true);
        let calls = reply["result"]["structuredContent"]["data"]["calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls.len(), 1, "required call lost under {fault}: {reply}");
        assert_eq!(calls[0]["calleeText"], "Target");
        assert_eq!(calls[0]["disposition"], "unresolved");
        assert_eq!(
            calls[0]["nativeCandidates"]["items"],
            json!([]),
            "{fault}: {reply}"
        );
        assert_eq!(
            calls[0]["nativeCandidates"]["omitted"], 0,
            "{fault}: {reply}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_corrupt_required_owner_or_call_refuses_whole_result() {
    for fault in [
        "missing_required_owner_header",
        "missing_required_owner_coverage",
        "invalid_required_call_id",
    ] {
        let reply = mcp_outgoing_corrupt_boundary_fixture(fault);
        tool(&reply, Some("store_unavailable"), true);
        let envelope = &reply["result"]["structuredContent"];
        assert!(
            envelope.get("data").is_none(),
            "corrupt required evidence yielded partial data: {fault}: {reply}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_prior_revision_target_is_not_a_selected_candidate() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("owner.js"), "function Owner() { Target(); }\n").unwrap();
    let target = root.join("target.js");
    fs::write(&target, "function Target() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = store.publication_for_tests().unwrap();
    let publish = |baseline| {
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &trellis::indexer::IndexOptions::new(root.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        store
            .publish_native(&graph, &capture, &native, &writer, baseline, &cancel)
            .unwrap()
    };
    let first = publish(store.index_baseline().unwrap());
    let selected = store.evidence_response().unwrap();
    let owner_id = selected
        .mcp_find("Owner", 20, None)
        .unwrap()
        .symbols
        .into_iter()
        .find(|s| s.declaration.name.as_deref() == Some("Owner"))
        .unwrap()
        .declaration
        .syntax_id;
    let first_calls = selected
        .mcp_outgoing(
            &owner_id,
            20,
            None,
            &|| std::time::Instant::now(),
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .unwrap()
        .unwrap();
    assert_eq!(first_calls.calls.len(), 1);
    assert_eq!(first_calls.calls[0].candidates.len(), 1);
    selected.finish(()).unwrap();
    drop(selected);
    fs::write(&target, "function Other() {}\n").unwrap();
    let second = publish(first);
    assert_ne!(first, second);
    drop(writer);
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let db = rusqlite::Connection::open(topology.index_db(&identity)).unwrap();
    let prior: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM native_version_declarations WHERE name='Target'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(prior > 0, "fixture must retain prior Target records");
    drop(db);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(303, &identity).unwrap();
    registry.activate(&identity.root_key).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 303, None);
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(2),
            "trellis_inspect",
            json!({
                "schemaVersion":1,"symbolId":owner_id,"view":"outgoing_calls"
            })
        )
    )
    .unwrap();
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let calls = reply["result"]["structuredContent"]["data"]["calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["nativeCandidates"]["items"],
        json!([]),
        "prior target leaked into selected revision: {reply}"
    );
    assert_eq!(calls[0]["nativeCandidates"]["omitted"], 0);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_foreign_source_set_is_not_an_optional_candidate() {
    let reply = mcp_outgoing_corrupt_boundary_fixture("foreign_source_set");
    tool(&reply, None, true);
    let calls = reply["result"]["structuredContent"]["data"]["calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0]["nativeCandidates"]["items"],
        json!([]),
        "foreign source set leaked: {reply}"
    );
    assert_eq!(calls[0]["nativeCandidates"]["omitted"], 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_source_selected_cached_bytes_not_disk() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "a\r\nb\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    drop(store);
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(143, &identity).unwrap();
    registry.activate(&identity.root_key).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 143, None);
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(1),
            "trellis_read_source",
            json!({"schemaVersion":1,"path":"a.js","startLine":1,"endLine":1})
        )
    )
    .unwrap();
    let response = next_reply(&mut reader);
    tool(&response, None, true);
    let data = &response["result"]["structuredContent"]["data"];
    assert_eq!(data["text"], "a\r\n");
    assert_eq!(data["byteRange"], json!({"start":0,"end":3}));
    assert_eq!(data["path"], "a.js");
    assert_eq!(data["startLine"], 1);
    assert_eq!(data["endLine"], 1);
    assert_eq!(
        data["evidence"],
        json!({"evidenceTier":"syntax",
        "semanticBasis":null,"freshness":"fresh","staleBecause":[]})
    );
    assert_eq!(response["result"]["structuredContent"]["partial"], false);
    assert_eq!(response["result"]["structuredContent"]["truncated"], false);
    let good_hash = data["contentHash"].clone();
    for (id, args, code, hash) in [
        (
            2,
            json!({"schemaVersion":1,"path":"missing.js","startLine":1,"endLine":1}),
            "not_found",
            None,
        ),
        (
            3,
            json!({"schemaVersion":1,"path":"a.js","startLine":3,"endLine":3,
            "expectedContentHash":"0".repeat(64)}),
            "revision_conflict",
            Some(good_hash),
        ),
        (
            4,
            json!({"schemaVersion":1,"path":"a.js","startLine":3,"endLine":3}),
            "range_too_large",
            None,
        ),
    ] {
        writeln!(
            client,
            "{}",
            call(false, json!(id), "trellis_read_source", args)
        )
        .unwrap();
        let reply = next_reply(&mut reader);
        tool(&reply, Some(code), true);
        let envelope = &reply["result"]["structuredContent"];
        assert_eq!(
            envelope["error"]["currentContentHash"],
            hash.unwrap_or(Value::Null)
        );
        assert!(envelope["error"]["currentBasis"].is_object());
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_source_post_validation_invalid_bytes_only_selected_range_fails() {
    use sha2::{Digest, Sha256};
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "a\nb\n").unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(144, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response()
                && response.finish(()).is_ok()
                && !catching_up
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("runtime did not finish valid startup before source mutation");
    let writer = store.publication_for_tests().unwrap();
    let malformed = [0x61, 0x0a, 0xff, 0x0a];
    store
        .mcp_replace_validated_source_bytes_for_tests(&writer, "a.js", &malformed)
        .unwrap();
    drop(writer);
    let response = store.evidence_response().unwrap();
    let good = response
        .mcp_source("a.js", 1, 1, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(good.text, "a\n");
    assert_eq!((good.byte_start, good.byte_end), (0, 2));
    assert_eq!(
        good.content_hash,
        "ead0215fe443756c2739e50b368ff79c283d540214d4ef503350fa25103198be"
    );
    assert_eq!(good.content_hash, hex::encode(Sha256::digest(malformed)));
    let wrong = response
        .mcp_source("a.js", 2, 2, None, Some(&"0".repeat(64)))
        .err()
        .unwrap();
    assert!(
        wrong.to_string().starts_with("revision conflict:"),
        "{wrong:#}"
    );
    let bad = response.mcp_source("a.js", 2, 2, None, None).err().unwrap();
    assert!(
        bad.to_string().starts_with("unsupported_encoding:"),
        "{bad:#}"
    );
    response.finish(()).unwrap();
    assert!(
        response.source_at("a.js", None).is_err(),
        "public attestation must not weaken"
    );
    drop(response);
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    for legacy in [false, true] {
        let (mut client, mut reader, session) = socket_session(&identity, &registry, 144, None);
        if legacy {
            writeln!(
                client,
                "{}",
                request(
                    json!(90),
                    "initialize",
                    fixture()["legacyInitialize"].clone()
                )
            )
            .unwrap();
            assert_eq!(
                next_reply(&mut reader)["result"]["protocolVersion"],
                "2025-11-25"
            );
            writeln!(
                client,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .unwrap();
        }
        let read = |id, line, expected_hash: Option<String>| {
            let mut args = json!({"schemaVersion":1,"path":"a.js","startLine":line,"endLine":line});
            if let Some(hash) = expected_hash {
                args["expectedContentHash"] = json!(hash);
            }
            call(legacy, json!(id), "trellis_read_source", args)
        };
        writeln!(client, "{}", read(1, 1, None)).unwrap();
        let valid = next_reply(&mut reader);
        tool(&valid, None, !legacy);
        let data = &valid["result"]["structuredContent"]["data"];
        assert_eq!(data["text"], "a\n", "{valid}");
        assert_eq!(data["byteRange"], json!({"start":0,"end":2}));
        assert_eq!(data["contentHash"], hex::encode(Sha256::digest(malformed)));
        let basis = valid["result"]["structuredContent"]["evidenceBasis"].clone();
        writeln!(client, "{}", read(2, 2, Some("0".repeat(64)))).unwrap();
        let conflict = next_reply(&mut reader);
        tool(&conflict, Some("revision_conflict"), !legacy);
        assert_eq!(
            conflict["result"]["structuredContent"]["error"]["currentBasis"],
            basis
        );
        assert_eq!(
            conflict["result"]["structuredContent"]["error"]["currentContentHash"],
            data["contentHash"]
        );
        writeln!(client, "{}", read(3, 2, None)).unwrap();
        let invalid = next_reply(&mut reader);
        tool(&invalid, Some("unsupported_encoding"), !legacy);
        let envelope = &invalid["result"]["structuredContent"];
        assert_eq!(envelope["error"]["retryable"], false);
        assert_eq!(envelope["error"]["currentBasis"], basis);
        assert_eq!(envelope["error"]["currentContentHash"], Value::Null);
        assert!(envelope.get("data").is_none(), "{invalid}");
        assert!(
            !invalid.to_string().contains('�'),
            "replacement character: {invalid}"
        );
        assert!(
            !invalid.to_string().contains("repair"),
            "repair text: {invalid}"
        );
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
}

#[test]
fn mcp_source_lf_ranges_exact_caps_and_hash_before_range() {
    use trellis::store::Store;
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "a\r\nb\n").unwrap();
    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    fs::write(root.join("a.js"), "changed on disk\n").unwrap();
    let response = store.evidence_response().unwrap();
    let first = response
        .mcp_source("a.js", 1, 1, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(first.text, "a\r\n");
    assert_eq!((first.byte_start, first.byte_end), (0, 3));
    let second = response
        .mcp_source("a.js", 2, 2, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(second.text, "b\n");
    assert_eq!((second.byte_start, second.byte_end), (3, 5));
    for (start, end) in [(3, 3), (1, 3), (0, 1), (1, 201)] {
        assert!(response.mcp_source("a.js", start, end, None, None).is_err());
    }
    assert!(
        response
            .mcp_source("missing.js", 1, 1, None, None)
            .unwrap()
            .is_none()
    );
    let wrong = response
        .mcp_source("a.js", 3, 3, None, Some(&"0".repeat(64)))
        .err()
        .unwrap();
    assert!(
        wrong.to_string().starts_with("revision conflict:"),
        "{wrong:#}"
    );
    let wrong_pin = trellis::model::IndexPin {
        index_generation: first.pin.index_generation,
        index_revision: first.pin.index_revision + 1,
    };
    let pin_error = response
        .mcp_source("missing.js", 1, 1, Some(wrong_pin), None)
        .err()
        .unwrap();
    assert!(pin_error.to_string().starts_with("revision conflict:"));
    response.finish(()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_source_required_selected_path_and_digest_refuse_corrupt_rows() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    for fault in ["missing_scope", "bad_digest"] {
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        fs::write(root.join("a.js"), "a\n").unwrap();
        let store = Store::open_for_tests(temp.path(), &root).unwrap();
        store.mcp_prepare_virgin_queue_for_tests().unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &trellis::indexer::IndexOptions::new(root.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let writer = store.publication_for_tests().unwrap();
        store
            .publish_native(
                &graph,
                &capture,
                &native,
                &writer,
                store.index_baseline().unwrap(),
                &cancel,
            )
            .unwrap();
        drop(writer);
        let before = store.evidence_response().unwrap();
        assert_eq!(
            before
                .mcp_source("a.js", 1, 1, None, None)
                .unwrap()
                .unwrap()
                .text,
            "a\n"
        );
        before.finish(()).unwrap();
        drop(before);
        let identity = WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .attach_marker()
            .unwrap();
        let topology =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let mut registry = CheckoutRegistry::with_roots(topology);
        registry.attach_launch(145, &identity).unwrap();
        let runtime = registry.activate(&identity.root_key).unwrap();
        tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
            loop {
                if let Ok((response, catching_up)) = runtime.evidence_response()
                    && response.finish(()).is_ok()
                    && !catching_up
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("runtime did not finish valid startup before corruption");
        let writer = store.publication_for_tests().unwrap();
        store.mcp_corrupt_find_for_tests(&writer, fault).unwrap();
        drop(writer);
        let selected = store.evidence_response().unwrap();
        let error = selected.mcp_source("a.js", 1, 1, None, None).err().unwrap();
        assert!(
            error.to_string().contains("store_unavailable"),
            "{fault}: {error:#}"
        );
        drop(selected);
        let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
        for legacy in [false, true] {
            let (mut client, mut reader, session) = socket_session(&identity, &registry, 145, None);
            if legacy {
                writeln!(
                    client,
                    "{}",
                    request(
                        json!(90),
                        "initialize",
                        fixture()["legacyInitialize"].clone()
                    )
                )
                .unwrap();
                assert_eq!(
                    next_reply(&mut reader)["result"]["protocolVersion"],
                    "2025-11-25"
                );
                writeln!(
                    client,
                    "{}",
                    json!({"jsonrpc":"2.0","method":"notifications/initialized"})
                )
                .unwrap();
            }
            writeln!(
                client,
                "{}",
                call(
                    legacy,
                    json!(1),
                    "trellis_read_source",
                    json!({"schemaVersion":1,"path":"a.js","startLine":1,"endLine":1})
                )
            )
            .unwrap();
            let reply = next_reply(&mut reader);
            tool(&reply, Some("store_unavailable"), !legacy);
            let envelope = &reply["result"]["structuredContent"];
            assert_eq!(
                envelope["error"]["currentBasis"],
                Value::Null,
                "{fault}: {reply}"
            );
            assert_eq!(
                envelope["error"]["currentContentHash"],
                Value::Null,
                "{fault}: {reply}"
            );
            assert!(envelope.get("data").is_none(), "{fault}: {reply}");
            drop(reader);
            drop(client);
            session.join().unwrap().unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_source_cold_coordinator_admits_prior_head_then_rejects_stale_pin() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let index = topology.index_db(&identity);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(293, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 293, None);
    let call = |id: i64| {
        modern(
            json!(id),
            "tools/call",
            json!({
                "name":"trellis_read_source","arguments":{"schemaVersion":1,"path":"a.js","startLine":1,"endLine":1}
            }),
        )
    };
    writeln!(client, "{}", call(1)).unwrap();
    tool(&next_reply(&mut reader), Some("index_not_ready"), true);
    assert!(
        !index.exists(),
        "cold W5 created derived state without a prior head"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();

    let store = Store::open_for_tests(temp.path(), &root).unwrap();
    store.mcp_prepare_virgin_queue_for_tests().unwrap();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
        &trellis::indexer::IndexOptions::new(root.clone()),
        store.root_id(),
        &cancel,
        |_| {},
    )
    .unwrap();
    let writer = store.publication_for_tests().unwrap();
    let pin = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    drop(writer);
    drop(store);
    fs::write(root.join("a.js"), "function Changed() {}\n").unwrap();
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let (release_tx, release_rx) = mpsc::sync_channel(1);
    let release_rx = std::sync::Mutex::new(release_rx);
    {
        let mut registry = registry.lock().await;
        registry.attach_launch(294, &identity).unwrap();
        assert!(registry.runtime(&identity.root_key).is_none());
        registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap();
        }));
    }
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 294, None);
    writeln!(client, "{}", call(2)).unwrap();
    entered_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["catchingUp"], true);
    assert_eq!(envelope["data"]["text"], "function Alpha() {}\n");
    assert_eq!(
        envelope["evidenceBasis"],
        json!({
            "indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision
        })
    );
    release_tx.send(()).unwrap();
    let runtime = registry.lock().await.runtime(&identity.root_key).unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            if let Ok((response, catching_up)) = runtime.evidence_response() {
                let now = response.status().unwrap().revision;
                if response.finish(()).is_ok() && !catching_up && now != pin {
                    break;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("new H/watch source pin did not settle");
    let mut stale = call(3);
    stale["params"]["arguments"]["expectedBasis"] = json!({
        "indexGeneration":pin.index_generation.to_string(), "indexRevision":pin.index_revision
    });
    writeln!(client, "{stale}").unwrap();
    let conflict = next_reply(&mut reader);
    tool(&conflict, Some("revision_conflict"), true);
    let current = &conflict["result"]["structuredContent"]["error"]["currentBasis"];
    assert_ne!(current, &stale["params"]["arguments"]["expectedBasis"]);
    assert_eq!(
        conflict["result"]["structuredContent"]["error"]["currentContentHash"],
        Value::Null
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_source_linked_selected_success_cancel_and_final_root() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, linked, registry, runtime) = linked_find_registry(temp.path(), 291);
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 291, None);
    let selected_call = |id: i64| {
        modern(
            json!(id),
            "tools/call",
            json!({
                "name":"trellis_read_source",
                "arguments":{"schemaVersion":1,"workspace":linked,"path":"a.js","startLine":1,"endLine":1}
            }),
        )
    };
    writeln!(client, "{}", selected_call(1)).unwrap();
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["workspace"], json!(linked), "{reply}");
    assert!(envelope["catchingUp"].is_boolean());
    assert_eq!(envelope["data"]["text"], "function held() {}\n");
    assert_eq!(envelope["data"]["path"], "a.js");
    assert!(envelope["data"]["contentHash"].as_str().is_some());
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();

    let cancel_gate = ReadGate::default();
    runtime.set_mcp_admitted_gate_for_tests(cancel_gate.hook());
    let (read_tx, read_rx) = mpsc::sync_channel(1);
    let frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let frame_count = frames.clone();
    let hooks = trellis::mcp::TransportHooksForTests {
        frame_read: Some(std::sync::Arc::new(move || {
            if frame_count.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                let _ = read_tx.try_send(());
            }
        })),
        ..Default::default()
    };
    let (mut client, mut reader, session) = hooked_session(&identity, &registry, 291, None, hooks);
    writeln!(client, "{}", selected_call(4)).unwrap();
    let waiter = cancel_gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    writeln!(
        client,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":4}})
    )
    .unwrap();
    read_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    cancel_gate.open();
    writeln!(client, "{}", modern(json!(5), "tools/list", json!({}))).unwrap();
    let reply = next_reply(&mut reader);
    assert_eq!(
        reply["id"], 5,
        "cancelled W5 selected call emitted evidence: {reply}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();

    let final_gate = ReadGate::default();
    runtime.set_mcp_admitted_gate_for_tests(final_gate.hook());
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 291, None);
    writeln!(client, "{}", selected_call(3)).unwrap();
    let waiter = final_gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    fs::rename(&linked, temp.path().join("moved-linked")).unwrap();
    final_gate.open();
    let reply = next_reply(&mut reader);
    tool(&reply, Some("root_changed"), true);
    assert_eq!(
        reply["result"]["structuredContent"]["workspace"],
        json!(linked),
        "{reply}"
    );
    assert_eq!(
        reply["result"]["structuredContent"]["error"]["currentBasis"],
        Value::Null
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_source_linked_selected_deadline_after_admission_keeps_resolved_shape() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, linked, registry, runtime) = linked_find_registry(temp.path(), 295);
    let gate = ReadGate::default();
    runtime.set_mcp_admitted_gate_for_tests(gate.hook());
    // Hold the W5 read after admission, then advance its virtual deadline.
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 295, None);
    writeln!(
        client,
        "{}",
        modern(
            json!(1),
            "tools/call",
            json!({
                "name":"trellis_read_source",
                "arguments":{"schemaVersion":1,"workspace":linked,"path":"a.js","startLine":1,"endLine":1}
            })
        )
    )
    .unwrap();
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    // Admission was observed before the test clock advances. One retryable
    // read now returns to the worker loop, where the virtual deadline wins.
    runtime.advance_mcp_clock_for_tests(trellis::mcp::CALL_DEADLINE + Duration::from_secs(1));
    gate.open();
    let reply = next_reply(&mut reader);
    tool(&reply, Some("deadline_exceeded"), true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["workspace"], json!(linked), "{reply}");
    assert!(envelope["catchingUp"].is_boolean(), "{reply}");
    assert!(
        envelope["error"].get("attemptedWorkspace").is_none(),
        "{reply}"
    );
    assert!(envelope.get("data").is_none(), "{reply}");
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}
