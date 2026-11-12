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

const REVIEWED_MODEL_NAME: &str = "voyageai/voyage-4-nano";
const REVIEWED_MODEL_REVISION: &str = "5fb34e870f49f19e79c94d5e118f5ee730133279";

fn emitted_document<'a>(data: &'a Value, item: &Value) -> &'a Value {
    let doc = item["doc"].as_u64().expect("emitted doc index") as usize;
    let documents = data["documents"].as_array().expect("document table");
    let entry = &documents[doc];
    assert_eq!(entry["ref"], doc);
    entry
}

fn fixture() -> Value {
    serde_json::from_str(include_str!("../fixtures/mcp/protocol.json")).unwrap()
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
#[test]
fn reply_cap_admits_only_a_catalog_past_the_response_cap() {
    let frame = |result: Value| {
        let mut line =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":1,"result":result})).unwrap();
        line.push(b'\n');
        line
    };
    let large = "x".repeat(trellis::mcp::wire::RESPONSE_BYTES);
    let catalog = frame(json!({"tools":[{"name":large}]}));
    assert!(catalog.len() > trellis::mcp::wire::RESPONSE_BYTES);
    assert_reply_cap(&catalog);
    let tool_result = frame(json!({"structuredContent":{"data":large}}));
    assert!(std::panic::catch_unwind(|| assert_reply_cap(&tool_result)).is_err());
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
                assert_reply_cap(&line);
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
    let sent = trellis::testing::signal_daemon_marker(&marker, libc::SIGTERM).unwrap_or(false);
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
        Self::start_with_fixture_home(
            tempfile::tempdir().unwrap(),
            workspace,
            cwd,
            phase,
            None,
            &[],
        )
    }
    fn start_with_phase_env(
        workspace: Option<&Path>,
        phase: Option<(&str, &Path)>,
        env: &[(&str, &std::ffi::OsStr)],
    ) -> Self {
        Self::start_with_fixture_home(
            tempfile::tempdir().unwrap(),
            workspace,
            None,
            phase,
            None,
            env,
        )
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
        let peer = Self::start_with_fixture_home(home, workspace, None, None, Some(&socket), &[]);
        (peer, listener)
    }
    fn start_with_fixture_home(
        home: tempfile::TempDir,
        workspace: Option<&Path>,
        cwd: Option<&Path>,
        phase: Option<(&str, &Path)>,
        causal_socket: Option<&Path>,
        env: &[(&str, &std::ffi::OsStr)],
    ) -> Self {
        let stderr_path = home.path().join("mcp-stderr.log");
        let stderr = fs::File::create(&stderr_path).unwrap();
        let mut cmd = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
        cmd.arg("mcp")
            .current_dir(cwd.unwrap_or(home.path()))
            .env("HOME", home.path())
            .env("XDG_CACHE_HOME", home.path().join("cache"))
            .env("XDG_DATA_HOME", home.path().join("data"))
            .env("TRELLIS_SEARCH_BACKEND", "cpu")
            .env(
                "TRELLIS_TEST_DAEMON_PID_FILE",
                home.path().join("auto-daemon.pid"),
            )
            .env(
                crate::child::DAEMON_STDERR,
                home.path().join("daemon-stderr.log"),
            )
            .envs(env.iter().copied())
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
                if line.len() + take > trellis::mcp::wire::CATALOG_BYTES {
                    // Treat an unbounded line as a failed protocol response, never
                    // allocate an attacker-controlled amount of captured stdout.
                    let _ = tx.send(vec![0; trellis::mcp::wire::CATALOG_BYTES + 1]);
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
        assert_reply_cap(&raw);
        assert_eq!(raw.last(), Some(&b'\n'));
        assert!(
            !raw[..raw.len() - 1].contains(&b'\n'),
            "one JSON message per line"
        );
        raw
    }
    fn finish(mut self) -> (std::process::ExitStatus, String) {
        self.stop()
    }
    /// Close stdin, stop this HOME's daemon and reap the MCP child, keeping the Peer (and so
    /// its HOME) alive for the caller.
    fn stop(&mut self) -> (std::process::ExitStatus, String) {
        self.stdin.take();
        stop_fixture_daemon(self._home.path());
        let (status, expired) = bounded_group_wait(&mut self.child);
        self.reaped = true;
        assert!(!expired, "MCP subprocess exceeded 10-second deadline");
        let stderr = String::from_utf8_lossy(&capture_file(&self.stderr_path)).into_owned();
        (status, stderr)
    }
    /// [`Peer::finish`], asserting the MCP child succeeded. A failure carries the child's
    /// stderr and the tail of the daemon stderr it kept, read while HOME still exists.
    #[track_caller]
    fn assert_success(mut self, context: &str) {
        let (status, stderr) = self.stop();
        let output = std::process::Output {
            status,
            stdout: Vec::new(),
            stderr: stderr.into_bytes(),
        };
        let daemon = self._home.path().join("daemon-stderr.log");
        crate::child::assert_cli_success_with(&output, context, Some(&daemon));
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

// #744 review: a failed peer's message carries its daemon stderr tail, read while the
// peer's HOME still exists. A successful exit never reads it, so the peer is killed here.
#[test]
fn failed_peer_message_carries_the_daemon_stderr_tail() {
    let peer = Peer::start(None);
    let line = "fixture daemon stderr line 7f3a";
    fs::write(
        peer._home.path().join("daemon-stderr.log"),
        format!("{line}\n"),
    )
    .unwrap();
    unsafe { libc::kill(-(peer.child.id() as libc::pid_t), libc::SIGKILL) };
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        peer.assert_success("forced peer failure")
    }))
    .expect_err("a killed peer is a failure");
    let message = panic.downcast_ref::<String>().cloned().unwrap_or_default();
    assert!(
        message.contains("forced peer failure: child failed"),
        "{message}"
    );
    assert!(message.contains(line), "{message}");
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
    let mut decoy = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")))
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

/// Holds an auto-started daemon's first checkout startup reconciliation at its
/// pre-capture hook (`TRELLIS_TEST_DAEMON_STARTUP_SOCKET`), before anything can
/// be published, until `release`.
struct DaemonStartupHold {
    path: PathBuf,
    arrived: Receiver<std::os::unix::net::UnixStream>,
}
impl DaemonStartupHold {
    fn bind(dir: &Path) -> Self {
        let path = dir.join("startup.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (sender, arrived) = mpsc::channel();
        std::thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
                .unwrap();
            if stream.read_exact(&mut [0]).is_ok() {
                let _ = sender.send(stream);
            }
        });
        Self { path, arrived }
    }
    fn release(&self) {
        let mut stream = self
            .arrived
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .expect("daemon startup never reached its pre-publication hook");
        stream.write_all(&[1]).unwrap();
    }
}

fn checkout(base: &Path) -> PathBuf {
    let path = base.join("checkout");
    fs::create_dir_all(path.join(".git")).unwrap();
    path
}
fn request(id: Value, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
pub(crate) fn modern(id: Value, method: &str, fields: Value) -> Value {
    let mut p = fields.as_object().unwrap().clone();
    p.insert("_meta".into(), fixture()["modernMeta"].clone());
    request(id, method, Value::Object(p))
}
fn error(v: &Value, code: i64, id: Value) {
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], id);
    assert_eq!(v["error"]["code"], code, "{v}");
}
pub(crate) fn tool(v: &Value, code: Option<&str>, modern: bool) {
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["result"]["resultType"].as_str().is_some(), modern);
    assert_eq!(v["result"]["isError"], code.is_some(), "{v}");
    let envelope = &v["result"]["structuredContent"];
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.len() <= 512, "summary exceeds 512 UTF-8 bytes: {v}");
    assert_eq!(envelope["requestId"], v["id"]);
    let (name, expected) = if let Some(code) = code {
        // The whole reply, so an unexpected code shows its reason.
        assert_eq!(envelope["error"]["code"], code, "{v}");
        let (name, _) = text.split_once(" failed:").expect("tool failure summary");
        (
            name,
            format!(
                "{name} failed: {code}: {}",
                envelope["error"]["message"].as_str().unwrap()
            ),
        )
    } else {
        let data = &envelope["data"];
        let (name, count, items) = if data.get("languages").is_some() {
            ("trellis_workspace_describe", 1, "workspace")
        } else if let Some(candidates) = data.get("candidates") {
            (
                text.split_once(": ").unwrap().0,
                candidates.as_array().unwrap().len(),
                "candidates",
            )
        } else if let Some(symbols) = data.get("symbols") {
            (
                "trellis_find_symbols",
                symbols.as_array().unwrap().len(),
                "symbols",
            )
        } else if let Some(hits) = data.get("hits") {
            (
                "trellis_search_symbols",
                hits.as_array().unwrap().len(),
                "hits",
            )
        } else if data.get("referencesVersion").is_some() {
            (
                "trellis_references",
                data["production"].as_array().unwrap().len()
                    + data["tests"].as_array().unwrap().len(),
                "edges",
            )
        } else if data.get("rulesVersion").is_some() {
            (
                if text.starts_with("trellis_rules_explain:") {
                    "trellis_rules_explain"
                } else {
                    "trellis_rules_check"
                },
                data["instances"].as_array().unwrap().len(),
                "instances",
            )
        } else if let Some(endpoints) = data.get("endpoints") {
            (
                "trellis_endpoints",
                endpoints.as_array().unwrap().len(),
                "endpoints",
            )
        } else if data.get("declaration").is_some() {
            ("trellis_declaration", 1, "declaration")
        } else if let Some(result) = data.get("result") {
            (
                "trellis_call_graph",
                result["edges"].as_array().unwrap().len(),
                "edges",
            )
        } else if let Some(calls) = data.get("calls") {
            (
                "trellis_outgoing_calls",
                calls.as_array().unwrap().len(),
                "calls",
            )
        } else if let Some(callers) = data.get("callers") {
            (
                "trellis_incoming_calls",
                callers.as_array().unwrap().len(),
                "calls",
            )
        } else {
            (
                "trellis_read_source",
                (data["endLine"].as_u64().unwrap() - data["startLine"].as_u64().unwrap() + 1)
                    as usize,
                "lines",
            )
        };
        let truncated = if envelope["truncated"] == true {
            // #665: a byte-reduced call graph names what it left out.
            let omitted = &data["responseOmitted"];
            let detail = match (omitted["nodes"].as_u64(), omitted["edges"].as_u64()) {
                (Some(nodes), Some(edges)) if nodes + edges > 0 => format!(
                    " ({nodes} nodes, {edges} edges omitted{})",
                    if data.get("semanticBases").is_some() {
                        "; compact:true fits more"
                    } else {
                        ""
                    }
                ),
                _ => String::new(),
            };
            format!(
                ", truncated: {}{detail}",
                envelope["truncationReason"].as_str().unwrap()
            )
        } else {
            String::new()
        };
        // A count of one takes the singular item word (#167).
        let items = if count == 1 {
            items.strip_suffix('s').unwrap_or(items)
        } else {
            items
        };
        (
            name,
            format!("{name}: {count} {items}{truncated}; full result in structuredContent."),
        )
    };
    assert!(trellis::mcp::catalog::known(name));
    assert_eq!(text, expected);
    assert_eq!(v["result"]["content"].as_array().unwrap().len(), 1);
}
/// Validate actual tool envelopes, not the summary string, with each catalog JSON Schema.
pub(crate) fn validate_structured_content(pairs: &[Value]) {
    let script = r#"
const {createRequire} = require('node:module');
const {readFileSync} = require('node:fs');
const requireFromTests = createRequire(process.cwd() + '/tests/mcp/package.json');
const Ajv2020 = requireFromTests('ajv/dist/2020.js').default;
const ajv = new Ajv2020({strict: false, allErrors: true});
const catalog = JSON.parse(readFileSync('tests/fixtures/mcp/catalog.json', 'utf8'));
const inputs = JSON.parse(readFileSync(0, 'utf8'));
for (const {name, structuredContent} of inputs) {
    const schema = catalog.find(tool => tool.name === name)?.outputSchema;
    if (!schema) throw Error(`unknown schema: ${name}`);
    const validate = ajv.compile(schema);
    if (!validate(structuredContent)) throw Error(`${name}: ${JSON.stringify(validate.errors)}`);
    if (structuredContent.error?.code === 'invalid_request' && !('workspace' in structuredContent)) {
        if ('attemptedWorkspace' in structuredContent.error || 'catchingUp' in structuredContent)
            throw Error(`${name}: unexpected pre-resolution field`);
        const withWorkspaceOnly = {...structuredContent, workspace: '/selected'};
        const withAttempt = {...structuredContent,
            error: {...structuredContent.error, attemptedWorkspace: {kind: 'invalid_type', jsonType: 'array'}}};
        if (validate(withWorkspaceOnly))
            throw Error(`${name}: argument failure must not accept half-resolved workspace`);
        if (!validate(withAttempt))
            throw Error(`${name}: closed invalid-workspace failure shape missing`);
        const resolved = {...structuredContent, workspace: '/selected', catchingUp: false};
        if (!validate(resolved)) throw Error(`${name}: resolved Failure branch missing`);
        if (validate({...resolved, error: withAttempt.error}))
            throw Error(`${name}: resolved Failure must exclude attemptedWorkspace`);
        if (validate({...withAttempt, workspace: '/selected'}))
            throw Error(`${name}: invalid-workspace failure must exclude workspace`);
    }
}
"#;
    let mut child = Command::new("node")
        .args(["-e", script])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(&serde_json::to_vec(pairs).unwrap())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    crate::child::assert_cli_success(&output, "structuredContent schema validation");
}
/// #785: the golden accepts the classified rule evidence and rejects nothing the rules tools
/// will emit; every new field is optional there and closed in its own object.
#[test]
fn rules_envelopes_accept_classified_evidence_and_kind_counts() {
    let sid = |c: &str| format!("sid:v1:{}", c.repeat(32));
    let document = json!({"sourceSetId":"app","language":"rust","path":"src/low.rs"});
    let identity =
        json!({"sourceSetId":"app","language":"rust","path":"src/low.rs","sid":sid("a")});
    let destination =
        json!({"sourceSetId":"app","language":"rust","path":"src/top.rs","sid":sid("b")});
    let evidence = |extra: Value| {
        let mut row = json!({"document":document,"revisionId":"r","range":{"start":0,"end":4},
            "kind":"structuralReference","syntaxId":sid("c"),"callId":null,"provenanceId":"p",
            "caller":null,"callerReason":"syntax_only"});
        row.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        row
    };
    let unbound = evidence(
        json!({"lines":null,"relationKind":"reference","spelling":"crate::top::T",
        "owner":null,"target":null,"targetReason":"unresolved"}),
    );
    let bound = evidence(json!({"kind":"semanticBinding","lines":{"start":3,"end":3},
        "relationKind":"call","spelling":"target",
        "owner":{"syntaxId":sid("d"),"name":"run","stableLookupKey":"src/low.rs::run"},
        "target":{"syntaxId":sid("e"),"displayKey":"src/top.rs#target","document":
            {"sourceSetId":"app","language":"rust","path":"src/top.rs"},"lines":{"start":1,"end":2}},
        "targetReason":null}));
    let module_level = evidence(json!({"lines":{"start":1,"end":1},"relationKind":"import",
        "spelling":"crate::top","owner":{"syntaxId":sid("a"),"name":null,"stableLookupKey":null},
        "target":null,"targetReason":"structural_only"}));
    let constraint = json!({"mode":"require","kind":"calls","targetRole":"any","status":"conforms",
        "evidence":[evidence(json!({}))],"unresolved":[]});
    let instance = json!({"rule":"$layers","target":identity,"destination":destination,
        "sourceLayer":"low","destinationLayer":"top","status":"deviates","excepted":false,
        "exceptionReason":null,"constraints":[constraint],
        "evidence":[unbound,bound,module_level],"unresolved":[],"referenceCount":3,
        "omittedEvidence":0,"kindCounts":{"call":1,"type":0,"import":1,"reference":1}});
    let counts = json!({"comparable":1,"conforms":0,"deviates":1,"unknown":0,"excepted":0});
    let envelope = |page: Value| {
        json!({"schemaVersion":1,"requestId":"1",
            "evidenceBasis":{"indexGeneration":"11111111-1111-4111-8111-111111111111","indexRevision":1},
            "data":{"rulesVersion":"rules:v2","indexGeneration":"g","indexRevision":1,
                "rulesFile":{"path":".trellis/rules.toml","source":"checkout-config","contentHash":"0".repeat(64)},
                "instances":[instance],"testOnly":[],"counts":counts,"testCounts":counts,
                "apps":[{"name":"trellis","source":"manifest","manifest":"Cargo.toml","counts":counts}],"noApp":{"counts":counts},
                "unassignedFileCount":0,"unassigned":[],"partialFiles":[],"limitations":[],
                "externalPackages":[],"page":page,"semanticBases":[]},
            "warnings":[],"partial":false,"truncated":false,"truncationReason":null,
            "workspace":"/selected","catchingUp":false})
    };
    // #784: explain does not carry the per-app counts.
    let mut explain = envelope(json!({"offset":0,"nextOffset":null,"total":1}));
    explain["data"].as_object_mut().unwrap().remove("apps");
    explain["data"].as_object_mut().unwrap().remove("noApp");
    validate_structured_content(&[
        json!({"name":"trellis_rules_check","structuredContent":
            envelope(json!({"offset":0,"nextOffset":null,"total":1,"status":null}))}),
        json!({"name":"trellis_rules_explain","structuredContent":explain}),
    ]);
}

fn expected_envelope(
    id: &Value,
    code: Option<&str>,
    label: &str,
    root: &Value,
    home: &Path,
) -> Value {
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
        let models_dir = if cfg!(target_os = "macos") {
            home.join("Library/Application Support/dev.squashmerge.trellis/models")
        } else {
            home.join("data/trellis/models")
        };
        envelope["data"]["searchIndex"] = json!({
            "backend":"cpu","unavailableReason":"model_not_configured",
            "configuration":{
                "configFile":home.join(".config/trellis/config.toml"),
                "modelsDir":models_dir,
                "modelName":REVIEWED_MODEL_NAME,
                "modelRevision":REVIEWED_MODEL_REVISION,
                "modelPath":models_dir
                    .join(REVIEWED_MODEL_NAME)
                    .join(REVIEWED_MODEL_REVISION),
                "backend":"cpu",
                "sources":{"modelsDir":"default","model":"default","backend":"environment"}
            }
        });
        envelope["workspace"] = root.clone();
        envelope["catchingUp"] = json!(true);
        envelope
    }
}
fn exact_tool(v: &Value, code: Option<&str>, modern: bool, label: &str, home: &Path) {
    let root = &v["result"]["structuredContent"]["workspace"];
    let mut envelope = expected_envelope(&v["id"], code, label, root, home);
    if code.is_none() {
        let actual = &v["result"]["structuredContent"];
        let state = actual["data"]["indexState"].as_str().unwrap();
        assert!(matches!(state, "indexing" | "ready"), "{actual}");
        envelope["data"]["indexState"] = json!(state);
        envelope["catchingUp"] = actual["catchingUp"].clone();
        assert!(actual["catchingUp"].is_boolean());
        if state == "ready" {
            // #777: a ready describe projects the admitted head's search
            // coverage; the missing model decides it, with its reason.
            envelope["data"]["searchIndex"]["state"] = json!("unavailable");
            envelope["data"]["searchIndex"]["declarations"] = json!(0);
            envelope["data"]["searchIndex"]["embedded"] = json!(0);
            let languages = actual["data"]["languages"].as_array().unwrap();
            assert_eq!(languages.len(), 5);
            for (row, language) in
                languages
                    .iter()
                    .zip(["java", "rust", "python", "javascript", "typescript"])
            {
                assert_eq!(row["language"], language);
                let coverage = &row["coverage"];
                let count = |field: &str| coverage[field].as_u64().unwrap();
                assert_eq!(
                    count("selected"),
                    count("complete") + count("partial") + count("failed")
                );
                assert!(row["nativeCoverageTier"] == "syntax");
                // #528: no import, so no language reports selected semantic evidence.
                assert_eq!(
                    row["semantic"],
                    json!({"available":false,"producerId":null,"freshness":"unavailable",
                        "coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},
                        "documents":[],
                        "note":"no semantic producer published; showing syntax","partialReasons":null,"reimport":null})
                );
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
            // #851: a ready head lists its apps once the owner has its architecture.
            assert!(
                matches!(
                    actual["data"]["apps"]["state"].as_str(),
                    Some("ready" | "building")
                ),
                "{actual}"
            );
            envelope["data"]["apps"] = actual["data"]["apps"].clone();
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
    tool(v, code, modern);
    let text = &v["result"]["content"][0]["text"];
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
    tool(v, Some("workspace_selection_failed"), true);
    let result = json!({"resultType":"complete","isError":true,
        "structuredContent":envelope,
        "content":[{"type":"text","text":v["result"]["content"][0]["text"]}]});
    assert_eq!(*v, json!({"jsonrpc":"2.0","id":id,"result":result}));
}
fn exact_tool_for_id(
    v: &Value,
    expected_id: &Value,
    code: Option<&str>,
    modern: bool,
    label: &str,
    home: &Path,
) {
    assert_eq!(&v["id"], expected_id, "outer JSON-RPC ID must echo sent ID");
    assert_eq!(
        &v["result"]["structuredContent"]["requestId"], expected_id,
        "typed requestId must echo sent ID"
    );
    exact_tool(v, code, modern, label, home);
}
/// Exact post-H golden responses retain full structured evidence and a summary.
fn exact_golden_settled(v: &Value, expected_id: &Value, modern: bool, envelope: Value) {
    assert_eq!(&v["id"], expected_id, "outer JSON-RPC ID must echo sent ID");
    assert_eq!(envelope["requestId"], *expected_id);
    let code = envelope["error"]["code"].as_str();
    tool(v, code, modern);
    let text = &v["result"]["content"][0]["text"];
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

/// Find's four scope rows for a checkout with no native documents (#156).
fn golden_settled_empty_find(id: &Value, root: &Value, pin: &Value) -> Value {
    json!({"schemaVersion":1,"requestId":id,"workspace":root,"catchingUp":false,
        "evidenceBasis":pin,"data":{"symbols":[],"documents":[],"coverage":[],
            "coverageDiagnostics":[],"scopeCoverage":[],"provenance":[]},
        "warnings":[{"code":"syntaxOnly","message":"This answer is syntax-derived",
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
    phase_barrier_before_reply(listener, stage, None, mutate);
}

fn phase_barrier_before_reply(
    listener: &std::os::unix::net::UnixListener,
    stage: &str,
    replies: Option<&Receiver<Value>>,
    mutate: impl FnOnce(),
) {
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                if let Some(replies) = replies {
                    match replies.try_recv() {
                        Ok(reply) => panic!("MCP replied before phase {stage}: {reply}"),
                        Err(mpsc::TryRecvError::Disconnected) => {
                            panic!("MCP reply reader ended before phase {stage}")
                        }
                        Err(mpsc::TryRecvError::Empty) => {}
                    }
                }
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
        // #478: the auto-started daemon indexes this virgin checkout. Its startup
        // reconciliation, before the first publication, is held until every
        // not-ready assertion is done, so no head can be published first.
        let startup = DaemonStartupHold::bind(tmp.path());
        let mut peer = Peer::start_with_phase_env(
            Some(&root),
            None,
            &[(
                "TRELLIS_TEST_DAEMON_STARTUP_SOCKET",
                startup.path.as_os_str(),
            )],
        );
        if legacy {
            let init = peer.ask(request(
                json!(1),
                "initialize",
                fixture()["legacyInitialize"].clone(),
            ));
            assert_eq!(
                init["result"],
                json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{},"prompts":{}},"serverInfo":{"name":"trellis","version":env!("CARGO_PKG_VERSION")}})
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
                json!({"resultType":"complete","ttlMs":0,"cacheScope":"private","supportedVersions":["2026-07-28","2025-11-25"],"capabilities":{"tools":{},"prompts":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"trellis","version":env!("CARGO_PKG_VERSION")}}})
            );
        }
        let list = if legacy {
            request(json!("catalog"), "tools/list", json!({}))
        } else {
            modern(json!("catalog"), "tools/list", json!({}))
        };
        let response = peer.ask(list);
        let catalog: Value =
            serde_json::from_str(include_str!("../fixtures/mcp/catalog.json")).unwrap();
        assert_eq!(catalog.as_array().unwrap().len(), 13);
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
                exact_tool(&reply, None, !legacy, "checkout", peer._home.path());
                assert_eq!(
                    reply["result"]["structuredContent"]["data"]["workspaceLabel"],
                    "checkout"
                );
            }
        }
        // Every tool answered while the first publication was still held.
        startup.release();
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
        // The outgoing view's limit has the same 1-50 range, checked before
        // any lookup.
        let fields = json!({"name":"trellis_outgoing_calls","arguments":{"schemaVersion":1,
            "symbolId":format!("sid:v1:{}", "0".repeat(31) + "1"),"limit":51}});
        let reply = peer.ask(if legacy {
            request(json!(43), "tools/call", fields)
        } else {
            modern(json!(43), "tools/call", fields)
        });
        tool(&reply, Some("range_too_large"), !legacy);
        for (id, name, arguments) in [
            (
                44,
                "trellis_declaration",
                json!({"schemaVersion":1,"symbolId":format!("sid:v1:{}", "a".repeat(32)),"view":"declaration"}),
            ),
            (
                45,
                "trellis_declaration",
                json!({"schemaVersion":1,"symbolId":format!("sid:v1:{}", "a".repeat(32)),"limit":1}),
            ),
            (
                46,
                "trellis_outgoing_calls",
                json!({"schemaVersion":1,"symbolId":format!("sid:v1:{}", "a".repeat(32)),"view":"outgoing_calls"}),
            ),
        ] {
            let fields = json!({"name":name,"arguments":arguments});
            let reply = peer.ask(if legacy {
                request(json!(id), "tools/call", fields)
            } else {
                modern(json!(id), "tools/call", fields)
            });
            tool(&reply, Some("invalid_request"), !legacy);
        }
        let old = json!({"name":"trellis_inspect","arguments":{"schemaVersion":1}});
        error(
            &peer.ask(if legacy {
                request(json!(47), "tools/call", old)
            } else {
                modern(json!(47), "tools/call", old)
            }),
            -32602,
            json!(47),
        );
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
        peer.assert_success("trellis mcp");
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
        assert_eq!(v["result"]["tools"].as_array().unwrap().len(), 13);
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
            "trellis_declaration",
            json!({"schemaVersion":1,"symbolId":"sid:v1:x"}),
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
    peer.assert_success("trellis mcp");
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
        13
    );
    peer.assert_success("trellis mcp");
    // Topology overlap is refused before marker attachment.
    let overlap = tmp.path().join("managed-home");
    fs::create_dir_all(overlap.join(".git")).unwrap();
    let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
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
    peer.assert_success("trellis mcp");
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
    modern_peer.assert_success("trellis mcp");
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
    legacy_peer.assert_success("trellis mcp");
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
        peer.assert_success("trellis mcp");
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
            peer.assert_success("trellis mcp");
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
            peer.assert_success("trellis mcp");
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
        peer.assert_success("trellis mcp");
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
    peer.assert_success("trellis mcp");
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
        peer.assert_success("trellis mcp");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_lines_remain_when_source_line_or_byte_cap_refuses() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(
        root.join("lines.js"),
        format!("function BigLines() {{\n{}}}\n", " ;\n".repeat(201)),
    )
    .unwrap();
    fs::write(
        root.join("bytes.js"),
        format!("function BigBytes() {{ /*{}*/ }}\n", "x".repeat(16_390)),
    )
    .unwrap();
    let root_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .root_key;
    let (mut peer, witness) = Peer::start_with_causal_witness(Some(&root));
    tool(
        &peer.ask(call(
            false,
            json!(1),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        )),
        None,
        true,
    );
    let pin = await_settled_golden_h(&witness, &root_key).await;
    let mut old_hash = None;
    for (offset, name, path, too_many_lines) in [
        (0, "BigLines", "lines.js", true),
        (10, "BigBytes", "bytes.js", false),
    ] {
        let found = peer.ask(call(
            false,
            json!(2 + offset),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":name,"expectedBasis":pin}),
        ));
        tool(&found, None, true);
        let found_data = &found["result"]["structuredContent"]["data"];
        let symbol = found_data["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|symbol| symbol["name"] == name)
            .unwrap();
        let declaration = peer.ask(call(
            false,
            json!(3 + offset),
            "trellis_declaration",
            json!({"schemaVersion":1,"symbolId":symbol["symbolId"],"expectedBasis":pin}),
        ));
        tool(&declaration, None, true);
        let declaration_data = &declaration["result"]["structuredContent"]["data"];
        for (item_data, item) in [(found_data, symbol), (declaration_data, declaration_data)] {
            let version = emitted_document(item_data, item);
            if offset == 0 {
                old_hash = Some(version["contentHash"].clone());
            }
            assert_eq!(version["document"]["path"], path);
            let lines = &item["lines"];
            assert!(
                !lines.is_null(),
                "source caps must not erase selected lines"
            );
            if too_many_lines {
                assert!(lines["end"].as_u64().unwrap() - lines["start"].as_u64().unwrap() >= 200);
            } else {
                assert_eq!(lines, &json!({"start":1,"end":1}));
            }
            let source = peer.ask(call(
                false,
                json!(4 + offset),
                "trellis_read_source",
                json!({
                    "schemaVersion":1,"path":path,"startLine":lines["start"],"endLine":lines["end"],
                    "expectedBasis":pin,"expectedContentHash":version["contentHash"]
                }),
            ));
            tool(&source, Some("range_too_large"), true);
        }
    }
    fs::write(root.join("lines.js"), "function BigLines() {}\n").unwrap();
    let home = peer._home.path().to_path_buf();
    let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    command
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"));
    let indexed = bounded_output(command);
    crate::child::assert_cli_success(&indexed, "index");
    let publication: Value = serde_json::from_slice(&indexed.stdout).unwrap();
    let current = &publication["publishedRevision"];
    assert_ne!(current, &pin);
    let stale = peer.ask(call(
        false,
        json!(22),
        "trellis_read_source",
        json!({
            "schemaVersion":1,"path":"lines.js","startLine":1,"endLine":1,
            "expectedBasis":pin,"expectedContentHash":old_hash
        }),
    ));
    tool(&stale, Some("revision_conflict"), true);
    assert_eq!(
        stale["result"]["structuredContent"]["error"]["currentBasis"],
        *current
    );
    let wrong_hash = peer.ask(call(
        false,
        json!(23),
        "trellis_read_source",
        json!({
            "schemaVersion":1,"path":"lines.js","startLine":1,"endLine":1,
            "expectedBasis":current,"expectedContentHash":old_hash
        }),
    ));
    tool(&wrong_hash, Some("revision_conflict"), true);
    assert_ne!(
        wrong_hash["result"]["structuredContent"]["error"]["currentContentHash"],
        *old_hash.as_ref().unwrap()
    );
    peer.assert_success("trellis mcp");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn warm_relay_serves_both_new_tools_and_rejects_closed_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function Owner() { Target(); }\n").unwrap();
    fs::write(root.join("b.js"), "function Target() {}\n").unwrap();
    fs::create_dir(root.join("tests")).unwrap();
    fs::write(root.join("tests/checks.rs"), "fn TestHelper() {}\n").unwrap();
    let root_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .root_key;
    for legacy in [false, true] {
        let (mut peer, witness) = Peer::start_with_causal_witness(Some(&root));
        if legacy {
            ready_legacy(&mut peer);
        }
        let mut schema_pairs = Vec::new();
        let describe = peer.ask(call(
            legacy,
            json!(1),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        ));
        tool(&describe, None, !legacy);
        schema_pairs.push(json!({"name":"trellis_workspace_describe",
            "structuredContent":describe["result"]["structuredContent"]}));
        let pin = await_settled_golden_h(&witness, &root_key).await;
        let names: Vec<_> = peer.ask(if legacy {
            request(json!(2), "tools/list", json!({}))
        } else {
            modern(json!(2), "tools/list", json!({}))
        })["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect();
        let mut expected_names: Vec<_> = fixture()["names"]
            .as_array()
            .unwrap()
            .iter()
            .map(|name| name.as_str().unwrap().to_owned())
            .collect();
        expected_names.insert(2, "trellis_search_symbols".to_owned());
        expected_names.extend([
            "trellis_call_graph".to_owned(),
            "trellis_references".to_owned(),
            "trellis_rules_check".to_owned(),
            "trellis_rules_explain".to_owned(),
            "trellis_show".to_owned(),
            "trellis_endpoints".to_owned(),
        ]);
        assert_eq!(names, expected_names);
        let found = peer.ask(call(
            legacy,
            json!(3),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"Owner","expectedBasis":pin}),
        ));
        tool(&found, None, !legacy);
        schema_pairs.push(json!({"name":"trellis_find_symbols",
            "structuredContent":found["result"]["structuredContent"]}));
        assert_eq!(
            found["result"]["structuredContent"]["data"]["symbols"][0]["role"],
            "production"
        );
        let id = found["result"]["structuredContent"]["data"]["symbols"][0]["symbolId"].clone();
        let target_find = peer.ask(call(
            legacy,
            json!(12),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"Target","expectedBasis":pin}),
        ));
        tool(&target_find, None, !legacy);
        let target_id = target_find["result"]["structuredContent"]["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|symbol| symbol["name"] == "Target")
            .unwrap()["symbolId"]
            .clone();
        let selected_target = peer.ask(call(
            legacy,
            json!(13),
            "trellis_declaration",
            json!({"schemaVersion":1,"symbolId":target_id,"expectedBasis":pin}),
        ));
        tool(&selected_target, None, !legacy);
        let target_data = &selected_target["result"]["structuredContent"]["data"];
        assert_eq!(target_data["declaration"]["role"], "production");
        let target_version = emitted_document(target_data, target_data);
        let incoming = peer.ask(call(
            legacy,
            json!(17),
            "trellis_incoming_calls",
            json!({"schemaVersion":1,"symbolId":target_id,"expectedBasis":pin}),
        ));
        tool(&incoming, None, !legacy);
        schema_pairs.push(json!({"name":"trellis_incoming_calls",
            "structuredContent":incoming["result"]["structuredContent"]}));
        let incoming_data = &incoming["result"]["structuredContent"]["data"];
        assert_eq!(incoming_data["match"]["basis"], "name");
        assert_eq!(incoming_data["callers"].as_array().unwrap().len(), 1);
        assert_eq!(incoming_data["callers"][0]["matchedBy"], "name");
        assert_eq!(incoming_data["callers"][0]["callerRole"], "production");
        assert!(incoming_data["callers"][0]["binding"].is_null());
        assert!(incoming_data["callers"][0]["targetId"].is_null());
        assert_eq!(incoming_data["scopeCoverage"].as_array().unwrap().len(), 5);
        assert_eq!(incoming_data["documents"].as_array().unwrap().len(), 1);
        assert_eq!(
            incoming["result"]["structuredContent"]["evidenceBasis"],
            pin
        );
        let test_find = peer.ask(call(
            legacy,
            json!(15),
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"TestHelper","expectedBasis":pin}),
        ));
        tool(&test_find, None, !legacy);
        schema_pairs.push(json!({"name":"trellis_find_symbols",
            "structuredContent":test_find["result"]["structuredContent"]}));
        let test_symbol = test_find["result"]["structuredContent"]["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|symbol| symbol["name"] == "TestHelper")
            .unwrap();
        assert_eq!(
            test_symbol["role"], "test",
            "test symbol in both MCP transports"
        );
        let test_declaration = peer.ask(call(
            legacy,
            json!(16),
            "trellis_declaration",
            json!({"schemaVersion":1,"symbolId":test_symbol["symbolId"],"expectedBasis":pin}),
        ));
        tool(&test_declaration, None, !legacy);
        schema_pairs.push(json!({"name":"trellis_declaration",
            "structuredContent":test_declaration["result"]["structuredContent"]}));
        assert_eq!(
            test_declaration["result"]["structuredContent"]["data"]["declaration"]["role"],
            "test"
        );
        for (number, name) in [(4, "trellis_declaration"), (5, "trellis_outgoing_calls")] {
            let reply = peer.ask(call(
                legacy,
                json!(number),
                name,
                json!({"schemaVersion":1,"symbolId":id,"expectedBasis":pin}),
            ));
            tool(&reply, None, !legacy);
            schema_pairs.push(json!({"name":name,
                "structuredContent":reply["result"]["structuredContent"]}));
            let data = &reply["result"]["structuredContent"]["data"];
            assert_eq!(reply["result"]["structuredContent"]["evidenceBasis"], pin);
            assert!(
                data["documents"]
                    .as_array()
                    .is_some_and(|docs| !docs.is_empty())
            );
            if name == "trellis_declaration" {
                assert_eq!(data["declaration"]["syntaxId"], id);
                assert_eq!(data["declaration"]["role"], "production");
            } else {
                assert_eq!(data["calls"].as_array().unwrap().len(), 1);
                assert_eq!(data["calls"][0]["calleeText"], "Target");
                let candidate = &data["calls"][0]["nativeCandidates"]["items"][0];
                assert_eq!(candidate["syntaxId"], target_id);
                let wire_version = emitted_document(data, candidate);
                for field in ["document", "revisionId", "contentHash", "provenanceId"] {
                    assert_eq!(wire_version[field], target_version[field]);
                }
                assert_ne!(
                    wire_version["provenanceId"],
                    emitted_document(data, &data["calls"][0])["provenanceId"]
                );
            }
        }
        for (number, name, field, value) in [
            (6, "trellis_declaration", "limit", json!(1)),
            (7, "trellis_declaration", "view", json!("declaration")),
            (8, "trellis_outgoing_calls", "view", json!("outgoing_calls")),
        ] {
            let mut args = json!({"schemaVersion":1,"symbolId":id});
            args[field] = value;
            tool(
                &peer.ask(call(legacy, json!(number), name, args)),
                Some("invalid_request"),
                !legacy,
            );
        }
        for (number, arguments, code) in [
            (
                30,
                json!({"schemaVersion":1,"symbolId":target_id,"view":"incoming_calls"}),
                "invalid_request",
            ),
            (
                31,
                json!({"schemaVersion":1,"symbolId":target_id,"limit":51}),
                "range_too_large",
            ),
            (
                32,
                json!({"schemaVersion":1,"symbolId":target_id,"expectedBasis":null}),
                "invalid_request",
            ),
            (
                33,
                json!({"schemaVersion":1,"symbolId":target_id,"workspace":null}),
                "invalid_request",
            ),
        ] {
            tool(
                &peer.ask(call(
                    legacy,
                    json!(number),
                    "trellis_incoming_calls",
                    arguments,
                )),
                Some(code),
                !legacy,
            );
        }
        for (number, limit) in [(9, 0), (10, 51)] {
            tool(
                &peer.ask(call(
                    legacy,
                    json!(number),
                    "trellis_outgoing_calls",
                    json!({"schemaVersion":1,"symbolId":id,"limit":limit}),
                )),
                Some("range_too_large"),
                !legacy,
            );
        }
        let source = peer.ask(call(
            legacy,
            json!(14),
            "trellis_read_source",
            json!({"schemaVersion":1,"path":"a.js","startLine":1,"endLine":1,
                "expectedBasis":pin,
                "expectedContentHash":found["result"]["structuredContent"]["data"]["documents"][0]["contentHash"]}),
        ));
        tool(&source, None, !legacy);
        schema_pairs.push(json!({"name":"trellis_read_source",
            "structuredContent":source["result"]["structuredContent"]}));
        for (index, name) in fixture()["names"].as_array().unwrap().iter().enumerate() {
            let name = name.as_str().unwrap();
            let failed = peer.ask(call(
                legacy,
                json!(20 + index),
                name,
                json!({"schemaVersion":1,"unexpected":true}),
            ));
            tool(&failed, Some("invalid_request"), !legacy);
            assert_eq!(
                failed["result"]["content"][0]["text"],
                format!(
                    "{name} failed: invalid_request: Invalid tool arguments: a field is not \
                     in this tool's inputSchema"
                ),
                "failure summary must identify the called tool"
            );
            schema_pairs.push(json!({"name":name,
                "structuredContent":failed["result"]["structuredContent"]}));
        }
        validate_structured_content(&schema_pairs);
        error(
            &peer.ask(call(
                legacy,
                json!(11),
                "trellis_inspect",
                json!({"schemaVersion":1,"symbolId":id,"view":"declaration"}),
            )),
            -32602,
            json!(11),
        );
        peer.assert_success("trellis mcp");
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
                exact_tool_for_id(
                    &invalid,
                    &id,
                    Some("invalid_request"),
                    !legacy,
                    "checkout",
                    peer._home.path(),
                );
                let args = fixture()["validArguments"][index].clone();
                let accepted = peer.ask(call(legacy, id.clone(), name, args));
                if index == 0 {
                    exact_tool_for_id(&accepted, &id, None, !legacy, "checkout", peer._home.path());
                    if settled_pin.is_none() {
                        settled_pin = Some(await_settled_golden_h(&witness, &root_key).await);
                    }
                } else {
                    let published = settled_pin.as_ref().expect("same-lineage H and watch ACK");
                    let workspace = &accepted["result"]["structuredContent"]["workspace"];
                    let expected = match index {
                        1 => golden_settled_empty_find(&id, workspace, published),
                        2 => golden_settled_failure(&id, workspace, "not_found", published.clone()),
                        3..=5 => {
                            golden_settled_failure(&id, workspace, "not_found", published.clone())
                        }
                        _ => unreachable!("fixed golden tool order"),
                    };
                    exact_golden_settled(&accepted, &id, !legacy, expected);
                }
            }
        }
        for name in &names {
            let name = name.as_str().unwrap();
            for args in [json!([]), json!(42), Value::Null] {
                // #769: non-object arguments say so; the rest is the golden envelope.
                let mut reply = peer.ask(call(legacy, json!(heavy_id), name, args));
                let message = "Tool arguments must be a JSON object";
                let error = &mut reply["result"]["structuredContent"]["error"]["message"];
                assert_eq!(*error, message);
                *error = fixture()["expected"]["failures"]["invalid_request"][0].clone();
                let text = &mut reply["result"]["content"][0]["text"];
                assert_eq!(*text, format!("{name} failed: invalid_request: {message}"));
                *text = json!(format!(
                    "{name} failed: invalid_request: {}",
                    fixture()["expected"]["failures"]["invalid_request"][0]
                        .as_str()
                        .unwrap()
                ));
                exact_tool_for_id(
                    &reply,
                    &json!(heavy_id),
                    Some("invalid_request"),
                    !legacy,
                    "checkout",
                    peer._home.path(),
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
                "trellis_outgoing_calls",
                json!({"schemaVersion":1,"symbolId":format!("sid:v1:{}","a".repeat(32)),"limit":50,"expectedBasis":pin}),
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
            peer._home.path(),
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
        peer.assert_success("trellis mcp");
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
        peer._home.path(),
    );
    peer.assert_success("trellis mcp");
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
        explicit._home.path(),
    );
    let ancestor_marker = ancestor.join(".git/trellis/workspace-id");
    let first_marker = fs::read(&ancestor_marker).unwrap();
    assert!(!nearer.join(".git/trellis/workspace-id").exists());
    explicit.assert_success("trellis mcp");
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
        adopted._home.path(),
    );
    adopted.assert_success("trellis mcp");
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
        implicit._home.path(),
    );
    let nearer_marker = fs::read(nearer.join(".git/trellis/workspace-id")).unwrap();
    assert_ne!(first_marker, nearer_marker);
    implicit.assert_success("trellis mcp");
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
    marker_lost.assert_success("trellis mcp");
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
    root_changed.assert_success("trellis mcp");
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
    peer.assert_success("trellis mcp");
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
    incomplete.assert_success("trellis mcp");
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
    completed.assert_success("trellis mcp");
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
        "trellis_declaration",
        "trellis_outgoing_calls",
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
        exact_tool(
            &response,
            Some("index_not_ready"),
            true,
            "checkout",
            peer._home.path(),
        );
    }
    no_db_under_except(peer._home.path(), &sentinels);
    for index in sentinels {
        assert_eq!(fs::read(&index).unwrap(), b"unusable-index-DO-NOT-OPEN");
        assert!(!index.with_extension("db-wal").exists());
        assert!(!index.with_extension("db-shm").exists());
    }
    assert!(!root.join(".git/trellis/requests.db").exists());
    peer.assert_success("trellis mcp");
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
        exact_tool(
            &response,
            None,
            mode == "modern",
            "checkout",
            peer._home.path(),
        );
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
            peer._home.path(),
        );
        peer.assert_success("trellis mcp");
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
            exact_tool(
                &reply,
                Some("index_not_ready"),
                !legacy,
                "checkout",
                peer._home.path(),
            );
            reply = peer.reply();
        }
        assert_eq!(reply["id"], "after-cancel");
        assert!(reply["result"].is_object());
        let (_tx, substitute) = mpsc::channel();
        let lines = std::mem::replace(&mut peer.lines, substitute);
        peer.assert_success("trellis mcp");
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
    peer.assert_success("trellis mcp");
    let all = drain_stdout(lines);
    assert!(all.len() <= 1, "EOF cannot cause duplicate outcomes");
    for line in all {
        assert_reply_cap(&line);
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
        exact_tool_for_id(&result, &json!(1), None, true, label, peer._home.path());
        let marker = git.join("trellis/workspace-id");
        ids.push(fs::read(&marker).unwrap());
        assert!(
            !root.join(".git/trellis/workspace-id").exists(),
            "pointer file must not be treated as directory"
        );
        peer.assert_success("trellis mcp");
        let mut reopened = Peer::start_in(None, Some(root));
        let result = reopened.ask(call(
            false,
            json!(2),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        ));
        exact_tool_for_id(&result, &json!(2), None, true, label, reopened._home.path());
        reopened.assert_success("trellis mcp");
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
        "reasons":["not_absolute","not_checkout","identity_changed","unavailable"]})
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
    let main = main.canonicalize().unwrap();
    let linked = linked.canonicalize().unwrap();
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
        // The worker may finish before this request samples catchingUp.
        assert!(selected["result"]["structuredContent"]["catchingUp"].is_boolean());
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
                assert_eq!(
                    *data,
                    json!({"symbols":[],"documents":[],"coverage":[],"coverageDiagnostics":[],
                        "scopeCoverage":[],"provenance":[]})
                );
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
        let relative = json!("relative");
        let answer = describe(
            &mut peer,
            12,
            json!({"schemaVersion":1,"workspace":relative}),
        );
        tool(&answer, Some("workspace_selection_failed"), !legacy);
        let envelope = &answer["result"]["structuredContent"];
        assert!(envelope.get("workspace").is_none());
        assert!(envelope.get("catchingUp").is_none());
        assert_eq!(envelope["error"]["reason"], "not_absolute");
        assert_eq!(envelope["error"]["attemptedWorkspace"]["value"], relative);
        assert_eq!(envelope["error"]["currentBasis"], Value::Null);
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
        peer.assert_success("trellis mcp");
    }
}

/// #839: a client launched in checkout A may select any verified local Git
/// checkout B, sharing no Git common directory with A: a separate `git init`
/// repository and a `git clone --local` copy. Each answer is B's evidence and
/// names B as its top-level `workspace`.
#[test]
fn per_call_selection_admits_unrelated_checkouts() {
    let temp = tempfile::tempdir().unwrap();
    let launch = temp.path().join("launch");
    git_selection(temp.path(), &["init", "--quiet", launch.to_str().unwrap()]);
    fs::write(
        launch.join("launch.py"),
        "def only_in_launch():\n    pass\n",
    )
    .unwrap();
    git_selection(&launch, &["add", "."]);
    git_selection(&launch, &["commit", "--quiet", "-m", "launch"]);
    let unrelated = temp.path().join("unrelated");
    git_selection(
        temp.path(),
        &["init", "--quiet", unrelated.to_str().unwrap()],
    );
    fs::write(
        unrelated.join("unrelated.py"),
        "def only_in_unrelated():\n    pass\n",
    )
    .unwrap();
    git_selection(&unrelated, &["add", "."]);
    git_selection(&unrelated, &["commit", "--quiet", "-m", "unrelated"]);
    let clone = temp.path().join("clone");
    git_selection(
        temp.path(),
        &[
            "clone",
            "--quiet",
            "--local",
            launch.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    // The clone has its own Git directory; an uncommitted edit is its own work.
    fs::write(clone.join("edit.py"), "def only_in_clone():\n    pass\n").unwrap();
    let launch = launch.canonicalize().unwrap();
    let unrelated = unrelated.canonicalize().unwrap();
    let clone = clone.canonicalize().unwrap();
    let mut peer = Peer::start(Some(&launch));
    let mut id = 10;
    for (selected, label, only, absent, file) in [
        (
            &unrelated,
            "unrelated",
            "only_in_unrelated",
            "only_in_launch",
            "unrelated.py",
        ),
        (
            &clone,
            "clone",
            "only_in_clone",
            "only_in_unrelated",
            "edit.py",
        ),
    ] {
        let mut ask = |name: &str, mut arguments: Value| {
            id += 1;
            arguments["workspace"] = json!(selected);
            let answer = peer.ask(call(false, json!(id), name, arguments));
            tool(&answer, None, true);
            let envelope = answer["result"]["structuredContent"].clone();
            assert_eq!(envelope["workspace"], json!(selected), "{answer}");
            envelope
        };
        let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
        loop {
            let status = ask("trellis_workspace_describe", json!({"schemaVersion":1}));
            assert_eq!(status["data"]["workspaceLabel"], label);
            if status["data"]["indexState"] == "ready" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "{label} checkout head was not admitted"
            );
            std::thread::yield_now();
        }
        let found = ask(
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":only}),
        );
        let symbols = found["data"]["symbols"].as_array().unwrap();
        assert!(
            symbols.iter().any(|symbol| symbol["name"] == only),
            "{found}"
        );
        let missing = ask(
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":absent}),
        );
        assert_eq!(missing["data"]["symbols"], json!([]), "{missing}");
        let source = ask(
            "trellis_read_source",
            json!({"schemaVersion":1,"path":file,"startLine":1,"endLine":1}),
        );
        assert!(source.to_string().contains(only), "{source}");
    }
    let back = peer.ask(call(
        false,
        json!(99),
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
    ));
    tool(&back, None, true);
    assert_eq!(
        back["result"]["structuredContent"]["workspace"],
        json!(launch)
    );
    peer.assert_success("trellis mcp");
}

#[test]
fn catalog_rejects_contradictory_unresolved_and_resolved_failures() {
    let catalog: Value =
        serde_json::from_str(include_str!("../fixtures/mcp/catalog.json")).unwrap();
    assert_eq!(catalog.as_array().unwrap().len(), 13);
    for entry in catalog.as_array().unwrap() {
        let defs = &entry["outputSchema"]["$defs"];
        // #864: one `Failure` per tool. An answering `workspace` requires `catchingUp` and
        // excludes the unresolved `attemptedWorkspace` and `reason`; without a workspace there
        // is no `catchingUp`.
        let failure = &defs["Failure"];
        assert_eq!(failure["if"], json!({"required":["workspace"]}));
        assert_eq!(failure["then"]["required"], json!(["catchingUp"]));
        let excluded = &failure["then"]["properties"]["error"]["properties"];
        assert_eq!(excluded["attemptedWorkspace"], json!(false));
        assert_eq!(excluded["reason"], json!(false));
        assert_eq!(failure["else"], json!({"not":{"required":["catchingUp"]}}));
        let error = &failure["properties"]["error"];
        assert_eq!(error["additionalProperties"], false);
        assert_eq!(
            error["properties"]["attemptedWorkspace"]["additionalProperties"],
            false
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
        peer.assert_success("daemon panic cannot interrupt thin client");
    }
}

#[test]
fn final_resolved_drift_is_attributed_for_five_tools_and_both_modes() {
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
                peer.assert_success("trellis mcp");
            }
        }
    }
}

/// A PATH whose `git` outlasts the 750 ms lookup bound while `flag` exists:
/// only the first run after it appears when `once`, every run otherwise.
/// Unstalled, the fixture's common-directory lookups are answered as fixed
/// fixture input, so a selection's own bounded lookups never wait on real Git
/// startup on a loaded host (#197); other commands run the real Git.
fn stalling_git_path(
    temp: &Path,
    flag: &Path,
    once: bool,
    checkouts: (&Path, &Path),
) -> std::ffi::OsString {
    let git = stalling_git(temp, flag, once, checkouts);
    let search = std::env::var_os("PATH").unwrap();
    std::env::join_paths(
        std::iter::once(git.parent().unwrap().to_path_buf()).chain(std::env::split_paths(&search)),
    )
    .unwrap()
}

/// The `git` executable [`stalling_git_path`] puts first on its PATH, for an
/// in-process fixture connection to run in place of the one on `PATH`.
fn stalling_git(temp: &Path, flag: &Path, once: bool, (main, linked): (&Path, &Path)) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let search = std::env::var_os("PATH").unwrap();
    let git = std::env::split_paths(&search)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let common = main.join(".git").canonicalize().unwrap();
    let armed = if once {
        format!("rm '{}' 2>/dev/null", flag.display())
    } else {
        format!("[ -e '{}' ]", flag.display())
    };
    let bin = temp.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("git"),
        format!(
            r#"#!/bin/sh
if {armed}; then exec sleep 3; fi
if [ "$#" -eq 5 ] && [ "$1" = -C ] &&
   [ "$3" = rev-parse ] && [ "$4" = --path-format=absolute ] &&
   [ "$5" = --git-common-dir ]; then
  case "$2" in '{}'|'{}') printf '%s\n' '{}'; exit 0 ;; esac
fi
exec '{}' "$@"
"#,
            main.display(),
            linked.display(),
            common.display(),
            git.display()
        ),
    )
    .unwrap();
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    bin.join("git")
}

/// #131: a final selected check whose Git lookup outlives its bound (a loaded
/// host) proves no root change. The call retries within its deadline and
/// answers; it was `root_changed`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_selected_check_survives_a_slow_git_lookup() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let (main, linked) = git_linked_fixture(temp.path());
    let identity = WorkspaceIdentity::discover(Some(&main), &main)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(301, &identity).unwrap();
    let selected = registry.select(301, &identity, &linked).unwrap();
    let runtime = registry.activate(&selected.identity.root_key).unwrap();
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
    .expect("linked checkout did not settle before the final-check fixture");
    runtime
        .freeze_mcp_call_clock_for_tests(std::time::Instant::now() - trellis::mcp::CALL_DEADLINE);
    let clock = runtime.clone();
    let hooks = trellis::mcp::TransportHooksForTests {
        paused_clock_for_tests: Some(std::sync::Arc::new(move || {
            clock.mcp_call_clock_now_for_tests()
        })),
        ..Default::default()
    };
    let slow = temp.path().join("slow-next-git");
    // Membership is fixed fixture input here; this test exercises final-check
    // retry, not real Git startup during the preceding bounded selection.
    let search = std::env::var_os("PATH").unwrap();
    let git = std::env::split_paths(&search)
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .expect("git on PATH");
    let common = main.join(".git").canonicalize().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::write(
        bin.join("git"),
        format!(
            r#"#!/bin/sh
if [ "$#" -eq 5 ] && [ "$1" = -C ] &&
   [ "$3" = rev-parse ] && [ "$4" = --path-format=absolute ] &&
   [ "$5" = --git-common-dir ]; then
  case "$2" in '{}'|'{}')
    if [ -e '{}' ] && rm '{}'; then exec sleep 3; fi
    printf '%s\n' '{}'
    exit 0 ;;
  esac
fi
exec '{}' "$@"
"#,
            main.display(),
            linked.display(),
            slow.display(),
            slow.display(),
            common.display(),
            git.display(),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(bin.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    let socket = temp.path().join("phase.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    // This connection's own barrier and Git, never process environment that
    // the other tests in this binary read (#869).
    let hooks = trellis::mcp::TransportHooksForTests {
        scope: trellis::testing::FixtureScope {
            phase: Some(trellis::testing::PhaseBarrier::new(
                "selected_final",
                &socket,
            )),
            git: Some(bin.join("git")),
        },
        ..hooks
    };
    struct FixtureGuard {
        client: Option<std::os::unix::net::UnixStream>,
        session: Option<std::thread::JoinHandle<std::io::Result<()>>>,
        reader: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for FixtureGuard {
        fn drop(&mut self) {
            if let Some(client) = &self.client {
                let _ = client.shutdown(std::net::Shutdown::Both);
            }
            if let Some(reader) = self.reader.take() {
                let _ = reader.join();
            }
            if let Some(session) = self.session.take() {
                let _ = session.join();
            }
        }
    }
    let mut cleanup = FixtureGuard {
        client: None,
        session: None,
        reader: None,
    };
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (client, mut reader, session) = hooked_session(&identity, &registry, 301, None, hooks);
    cleanup.client = Some(client);
    cleanup.session = Some(session);
    writeln!(
        cleanup.client.as_mut().unwrap(),
        "{}",
        call(
            false,
            json!(1),
            "trellis_workspace_describe",
            json!({"schemaVersion":1,"workspace":linked}),
        )
    )
    .unwrap();
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    cleanup.reader = Some(std::thread::spawn(move || {
        let _ = reply_tx.send(next_reply(&mut reader));
    }));
    phase_barrier_before_reply(&listener, "selected_final", Some(&reply_rx), || {
        fs::write(&slow, b"").unwrap()
    });
    let reply = reply_rx
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    cleanup.reader.take().unwrap().join().unwrap();
    assert!(!slow.exists(), "the final check ran no Git lookup");
    tool(&reply, None, true);
    assert_eq!(
        reply["result"]["structuredContent"]["workspace"],
        json!(linked)
    );
    cleanup
        .client
        .take()
        .unwrap()
        .shutdown(std::net::Shutdown::Both)
        .unwrap();
    cleanup.session.take().unwrap().join().unwrap().unwrap();
}

/// #131 review: a final selected check that keeps stalling waits off the
/// connection's decision path. Call B, finished while A's check stalls, is
/// answered at once; A is `deadline_exceeded` at its own deadline. A check
/// retried in place would answer A first and B only after A's deadline.
#[test]
fn a_stalled_final_check_does_not_hold_other_calls() {
    let temp = tempfile::tempdir().unwrap();
    let (main, linked) = git_linked_fixture(temp.path());
    let stalled = temp.path().join("stall-git");
    let path = stalling_git_path(temp.path(), &stalled, false, (&main, &linked));
    let socket = temp.path().join("phase.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let mut peer = Peer::start_with_phase_env(
        Some(&main),
        Some(("selected_final", &socket)),
        &[("PATH", path.as_os_str())],
    );
    let describe = |id: Value, workspace: Option<&Path>| {
        let mut arguments = json!({"schemaVersion":1});
        if let Some(workspace) = workspace {
            arguments["workspace"] = json!(workspace);
        }
        serde_json::to_string(&call(false, id, "trellis_workspace_describe", arguments)).unwrap()
    };
    // Warm both checkouts so neither later call races a cold activation.
    tool(
        &peer.ask(serde_json::from_str(&describe(json!(0), None)).unwrap()),
        None,
        true,
    );
    peer.send_raw(describe(json!(1), Some(&linked)).as_bytes());
    phase_barrier(&listener, "selected_final", || {});
    tool(&peer.reply(), None, true);
    // A reaches its final check; B is read and finishes while A's lookups stall.
    peer.send_raw(describe(json!("A"), Some(&linked)).as_bytes());
    phase_barrier(&listener, "selected_final", || {
        fs::write(&stalled, b"").unwrap();
        peer.send_raw(describe(json!("B"), None).as_bytes());
    });
    let first = peer.reply();
    let second = peer.reply();
    fs::remove_file(&stalled).unwrap();
    assert_eq!(
        first["id"], "B",
        "B waited behind A's stalled check: {first}"
    );
    tool(&first, None, true);
    assert_eq!(second["id"], "A", "{second}");
    tool(&second, Some("deadline_exceeded"), true);
    peer.assert_success("trellis mcp");
}

/// Serve every phase pause on `listener` until the test ends, each on its own
/// thread so one held pause never delays another. `hold` sees each pause's
/// stage and returns when to release it (`None`: at once).
fn serve_pauses(
    listener: &std::os::unix::net::UnixListener,
    hold: impl FnMut(&str) -> Option<std::time::Instant> + Send + 'static,
) {
    let listener = listener.try_clone().unwrap();
    let hold = std::sync::Arc::new(std::sync::Mutex::new(hold));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let hold = hold.clone();
            std::thread::spawn(move || {
                let mut stage = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut stage)
                    .unwrap();
                let at = (hold.lock().unwrap())(stage.trim_end());
                if let Some(at) = at {
                    std::thread::sleep(at.saturating_duration_since(std::time::Instant::now()));
                }
                let _ = stream.write_all(b"x");
            });
        }
    });
}

fn describe_frame(id: Value, workspace: Option<&Path>) -> Vec<u8> {
    let mut arguments = json!({"schemaVersion":1});
    if let Some(workspace) = workspace {
        arguments["workspace"] = json!(workspace);
    }
    serde_json::to_vec(&call(false, id, "trellis_workspace_describe", arguments)).unwrap()
}

/// Frozen call clocks and a parked final-check helper for the two deadline
/// ordering tests below. Every installed read gate is opened before teardown.
struct FinalRecheckFixture {
    stalled: PathBuf,
    linked: PathBuf,
    listener: std::os::unix::net::UnixListener,
    launch: std::sync::Arc<trellis::daemon::registry::CheckoutRuntime>,
    selected: std::sync::Arc<trellis::daemon::registry::CheckoutRuntime>,
    read_gate: Option<ReadGate>,
    client: Option<std::os::unix::net::UnixStream>,
    reader: Option<BufReader<std::os::unix::net::UnixStream>>,
    session: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

impl FinalRecheckFixture {
    async fn new(temp: &Path) -> Self {
        use trellis::daemon::registry::CheckoutRegistry;
        use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
        let (main, linked) = git_linked_fixture(temp);
        let identity = WorkspaceIdentity::discover(Some(&main), &main)
            .unwrap()
            .attach_marker()
            .unwrap();
        let topology = TopologyRoots::isolated_for_tests(temp.join("cache"), temp.join("data"));
        let mut registry = CheckoutRegistry::with_roots(topology);
        registry.attach_launch(302, &identity).unwrap();
        let launch = registry.activate(&identity.root_key).unwrap();
        let selection = registry.select(302, &identity, &linked).unwrap();
        let selected = registry.activate(&selection.identity.root_key).unwrap();
        for runtime in [&launch, &selected] {
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
            .expect("checkout did not settle before the final-recheck fixture");
        }
        let origin = std::time::Instant::now() - trellis::mcp::CALL_DEADLINE;
        launch.freeze_mcp_call_clock_for_tests(origin);
        selected.freeze_mcp_call_clock_for_tests(origin);
        let clock = launch.clone();
        let stalled = temp.join("stall-git");
        let socket = temp.join("phase.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // This connection's own barrier and Git, never process environment
        // that the other tests in this binary read (#869).
        let hooks = trellis::mcp::TransportHooksForTests {
            paused_clock_for_tests: Some(std::sync::Arc::new(move || {
                clock.mcp_call_clock_now_for_tests()
            })),
            scope: trellis::testing::FixtureScope {
                phase: Some(trellis::testing::PhaseBarrier::new(
                    "selected_final,final_recheck",
                    &socket,
                )),
                git: Some(stalling_git(temp, &stalled, false, (&main, &linked))),
            },
            ..Default::default()
        };
        let mut fixture = Self {
            stalled,
            linked,
            listener,
            launch,
            selected,
            read_gate: None,
            client: None,
            reader: None,
            session: None,
        };
        let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
        let (client, reader, session) = hooked_session(&identity, &registry, 302, None, hooks);
        fixture.client = Some(client);
        fixture.reader = Some(reader);
        fixture.session = Some(session);
        fixture
    }

    fn advance(&self, by: Duration) {
        self.launch.advance_mcp_clock_for_tests(by);
        self.selected.advance_mcp_clock_for_tests(by);
    }

    fn send(&mut self, frame: &Value) {
        writeln!(self.client.as_mut().unwrap(), "{frame}").unwrap();
    }

    fn reply(&mut self) -> Value {
        next_reply(self.reader.as_mut().unwrap())
    }

    fn start_a(&mut self) {
        self.send(
            &serde_json::from_slice(&describe_frame(json!("A"), Some(&self.linked))).unwrap(),
        );
        phase_barrier(&self.listener, "selected_final", || {
            fs::write(&self.stalled, b"").unwrap();
        });
    }

    fn expect_a_expired(&mut self) {
        // Advancing a paused clock does not wake the coordinator by itself.
        self.send(&modern(json!("wake"), "tools/list", json!({})));
        let mut replies = [self.reply(), self.reply()];
        replies.sort_by_key(|reply| reply["id"].as_str().unwrap().to_owned());
        let [a, wake] = replies;
        assert_eq!(a["id"], "A", "{a}");
        tool(&a, Some("deadline_exceeded"), true);
        assert_eq!(wake["id"], "wake", "{wake}");
        assert!(wake["result"]["tools"].is_array(), "{wake}");
    }

    fn finish(&mut self) {
        self.client
            .take()
            .unwrap()
            .shutdown(std::net::Shutdown::Both)
            .unwrap();
        self.session.take().unwrap().join().unwrap().unwrap();
    }
}

impl Drop for FinalRecheckFixture {
    fn drop(&mut self) {
        if let Some(gate) = &self.read_gate {
            gate.open();
        }
        let _ = fs::remove_file(&self.stalled);
        if let Some(client) = &self.client {
            let _ = client.shutdown(std::net::Shutdown::Both);
        }
        if let Some(session) = self.session.take() {
            let _ = session.join();
        }
    }
}

/// #195 review: A expires at its own deadline even while its unavailable
/// final check remains parked. No host-time response-speed bound is needed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_persistently_unavailable_final_check_is_decided_by_its_deadline() {
    let temp = tempfile::tempdir().unwrap();
    let mut fixture = FinalRecheckFixture::new(temp.path()).await;
    fixture.start_a();
    let listener = fixture.listener.try_clone().unwrap();
    phase_barrier(&listener, "final_recheck", || {
        fixture.advance(trellis::mcp::CALL_DEADLINE);
        fixture.expect_a_expired();
    });
    fixture.finish();
}

/// #195 review: B finishes near its own virtual deadline and is answered
/// while A's recheck is still parked, then A expires at its earlier deadline.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_finishing_near_its_deadline_is_not_held_behind_a_recheck() {
    let temp = tempfile::tempdir().unwrap();
    let mut fixture = FinalRecheckFixture::new(temp.path()).await;
    fixture.start_a();
    let listener = fixture.listener.try_clone().unwrap();
    phase_barrier(&listener, "final_recheck", || {
        fixture.advance(Duration::from_millis(100));
        let gate = ReadGate::default();
        fixture.read_gate = Some(gate.clone());
        fixture.launch.set_mcp_read_gate_for_tests(gate.hook());
        fixture.send(&serde_json::from_slice(&describe_frame(json!("B"), None)).unwrap());
        gate.await_held(1);
        // A expires at +5000 ms; B, accepted at +100 ms, at +5100 ms.
        fixture.advance(Duration::from_millis(4800));
        gate.open();
        let b = fixture.reply();
        assert_eq!(b["id"], "B", "{b}");
        tool(&b, None, true);
        fixture.advance(Duration::from_millis(100));
        fixture.expect_a_expired();
    });
    fixture.finish();
}

/// #195 re-review: no call's send decision waits on another call's final
/// identity lookup. B is accepted first and its worker held; A, accepted just
/// after, has its final check stalled by Git and held at `hold_stage`. Only
/// then is B's worker released, while A stays held. A check run on the
/// coordinator would keep B's decision behind A's held check, past B's
/// deadline; B must be answered with its result, and A is `deadline_exceeded`
/// at its own deadline. The overlap is ordered by hooks, not host time (#197).
fn earlier_call_is_decided_while_a_later_final_check_stalls(hold_stage: &'static str) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{Sender, channel};
    use std::sync::{Arc, Mutex};
    type Pause = (Sender<()>, Receiver<()>);
    let temp = tempfile::tempdir().unwrap();
    let (main, linked) = git_linked_fixture(temp.path());
    let stalled = temp.path().join("stall-git");
    let path = stalling_git_path(temp.path(), &stalled, false, (&main, &linked));
    let socket = temp.path().join("phase.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    // Each pause reports its arrival and waits for its own release.
    let (b_arrived, b_held) = channel();
    let (release_b, b_gate) = channel();
    let (a_arrived, a_held) = channel();
    let (release_a, a_gate) = channel();
    let pauses: Arc<Mutex<(Option<Pause>, Option<Pause>)>> = Arc::new(Mutex::new((
        Some((b_arrived, b_gate)),
        Some((a_arrived, a_gate)),
    )));
    // Armed once the warm-up calls are answered: B's is the next call worker.
    let armed = Arc::new(AtomicBool::new(false));
    let (flag, arm) = (stalled.clone(), armed.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let (pauses, flag, arm) = (pauses.clone(), flag.clone(), arm.clone());
            std::thread::spawn(move || {
                let mut stage = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut stage)
                    .unwrap();
                let stage = stage.trim_end();
                let pause = if !arm.load(Ordering::Acquire) {
                    None
                } else if stage == "call_worker" {
                    pauses.lock().unwrap().0.take()
                } else {
                    if stage == "selected_final" {
                        fs::write(&flag, b"").unwrap();
                    }
                    (stage == hold_stage)
                        .then(|| pauses.lock().unwrap().1.take())
                        .flatten()
                };
                if let Some((arrived, gate)) = pause {
                    arrived.send(()).unwrap();
                    let _ = gate.recv();
                }
                let _ = stream.write_all(b"x");
            });
        }
    });
    let mut peer = Peer::start_with_phase_env(
        Some(&main),
        Some(("call_worker,selected_final,final_recheck", &socket)),
        &[
            ("PATH", path.as_os_str()),
            ("TRELLIS_TEST_MCP_RELAY_DEADLINE_MS", "10000".as_ref()),
        ],
    );
    tool(
        &peer.ask(serde_json::from_slice(&describe_frame(json!(0), None)).unwrap()),
        None,
        true,
    );
    let warm = peer.ask(serde_json::from_slice(&describe_frame(json!(1), Some(&linked))).unwrap());
    tool(&warm, None, true);
    armed.store(true, Ordering::Release);
    peer.send_raw(&describe_frame(json!("B"), None));
    b_held
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .expect("B's worker never started");
    peer.send_raw(&describe_frame(json!("A"), Some(&linked)));
    a_held
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap_or_else(|_| panic!("A's final check never reached {hold_stage}"));
    release_b.send(()).unwrap();
    // A stays held: its decision can come only from its own deadline.
    let first = peer.reply();
    let second = peer.reply();
    release_a.send(()).unwrap();
    let _ = fs::remove_file(&stalled);
    let (b, a) = if first["id"] == "B" {
        (first, second)
    } else {
        (second, first)
    };
    assert_eq!((b["id"].as_str(), a["id"].as_str()), (Some("B"), Some("A")));
    tool(&b, None, true);
    tool(&a, Some("deadline_exceeded"), true);
    peer.assert_success("trellis mcp");
}

#[test]
fn an_earlier_call_is_decided_while_a_later_recheck_stalls() {
    earlier_call_is_decided_while_a_later_final_check_stalls("final_recheck");
}

#[test]
fn an_earlier_call_is_decided_while_a_later_first_final_check_stalls() {
    earlier_call_is_decided_while_a_later_final_check_stalls("selected_final");
}

const MCP_SELECTED_SEND_PHASE: &str = "selected_call_send";

/// #195 re-review: a selected call's identity is compared again at its send.
/// B's helper finds its checkout intact; the coordinator is then held at B's
/// send (standing in for any blocking work, such as writing to a slow client)
/// while B's root is moved away. B must answer `root_changed`, never the
/// success its earlier check saw.
#[test]
fn a_root_replaced_after_its_final_check_is_root_changed_at_send() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let temp = tempfile::tempdir().unwrap();
    let (main, linked) = git_linked_fixture(temp.path());
    let moved = temp.path().join("moved-linked");
    let socket = temp.path().join("phase.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let (arm, root, away) = (armed.clone(), linked.clone(), moved.clone());
    serve_pauses(&listener, move |stage| {
        if stage == MCP_SELECTED_SEND_PHASE && arm.swap(false, Ordering::AcqRel) {
            fs::rename(&root, &away).unwrap();
        }
        None
    });
    let mut peer =
        Peer::start_with_phase_env(Some(&main), Some((MCP_SELECTED_SEND_PHASE, &socket)), &[]);
    tool(
        &peer.ask(serde_json::from_slice(&describe_frame(json!(0), None)).unwrap()),
        None,
        true,
    );
    let warm = peer.ask(serde_json::from_slice(&describe_frame(json!(1), Some(&linked))).unwrap());
    tool(&warm, None, true);
    armed.store(true, Ordering::Release);
    let reply =
        peer.ask(serde_json::from_slice(&describe_frame(json!("B"), Some(&linked))).unwrap());
    assert!(moved.exists(), "B's send was never held: {reply}");
    fs::rename(&moved, &linked).unwrap();
    tool(&reply, Some("root_changed"), true);
    assert_eq!(
        reply["result"]["structuredContent"]["workspace"],
        json!(linked)
    );
    peer.assert_success("trellis mcp");
}

#[test]
fn a_selection_failure_does_not_reach_the_selected_send_phase() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let temp = tempfile::tempdir().unwrap();
    let (main, linked) = git_linked_fixture(temp.path());
    let missing = temp.path().canonicalize().unwrap().join("missing-selected");
    let moved = temp.path().join("moved-linked");
    let socket = temp.path().join("phase.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let (arm, root, away) = (armed.clone(), linked.clone(), moved.clone());
    serve_pauses(&listener, move |stage| {
        if stage == MCP_SELECTED_SEND_PHASE && arm.swap(false, Ordering::AcqRel) {
            fs::rename(&root, &away).unwrap();
        }
        None
    });
    let mut peer =
        Peer::start_with_phase_env(Some(&main), Some((MCP_SELECTED_SEND_PHASE, &socket)), &[]);
    tool(
        &peer.ask(serde_json::from_slice(&describe_frame(json!(0), None)).unwrap()),
        None,
        true,
    );
    armed.store(true, Ordering::Release);
    let reply =
        peer.ask(serde_json::from_slice(&describe_frame(json!(2), Some(&missing))).unwrap());
    exact_selection_failure(&reply, 2, "unavailable", &missing);
    assert!(
        !moved.exists(),
        "a preselection failure reached the selected send phase: {reply}"
    );
    assert!(linked.exists(), "the live rename target was moved: {reply}");
    peer.assert_success("trellis mcp");
}

#[test]
fn an_unreadable_common_dir_at_send_is_retried_not_root_changed() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let temp = tempfile::tempdir().unwrap();
    let (main, linked) = git_linked_fixture(temp.path());
    let common = main.join(".git");
    let socket = temp.path().join("phase.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let (armed, faulted, rechecked) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let (arm, fault, recheck, dir) = (
        armed.clone(),
        faulted.clone(),
        rechecked.clone(),
        common.clone(),
    );
    serve_pauses(&listener, move |stage| {
        // B's send-time stat of its common Git directory fails with EACCES,
        // an I/O error that proves nothing; its retried lookup then succeeds.
        if stage == "call_send" && arm.swap(false, Ordering::AcqRel) {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o000)).unwrap();
            fault.store(
                fs::metadata(dir.join("HEAD"))
                    .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied),
                Ordering::Release,
            );
        } else if stage == "final_recheck" && fault.load(Ordering::Acquire) {
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
            recheck.store(true, Ordering::Release);
        }
        None
    });
    let mut peer =
        Peer::start_with_phase_env(Some(&main), Some(("call_send,final_recheck", &socket)), &[]);
    tool(
        &peer.ask(serde_json::from_slice(&describe_frame(json!(0), None)).unwrap()),
        None,
        true,
    );
    let warm = peer.ask(serde_json::from_slice(&describe_frame(json!(1), Some(&linked))).unwrap());
    tool(&warm, None, true);
    armed.store(true, Ordering::Release);
    let reply =
        peer.ask(serde_json::from_slice(&describe_frame(json!("B"), Some(&linked))).unwrap());
    fs::set_permissions(&common, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        faulted.load(Ordering::Acquire),
        "the fault was not injected (running as root?): {reply}"
    );
    assert!(
        rechecked.load(Ordering::Acquire),
        "B was never rechecked: {reply}"
    );
    tool(&reply, None, true);
    peer.assert_success("trellis mcp");
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
    peer.assert_success("trellis mcp");
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
    minimum_request_seq: u64,
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
                let request_seq = event["requestSeq"].as_u64().expect("ACK request sequence");
                if event["pin"] == *published && request_seq >= self.minimum_request_seq {
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
    let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    command
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"));
    let indexed = bounded_output(command);
    crate::child::assert_cli_success(&indexed, "index");
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
    assert_eq!(described["data"]["languages"].as_array().unwrap().len(), 5);
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
    let mut events = Vec::new();
    // This fixture submits exactly one FIFO request. An earlier same-pin
    // startup ACK cannot certify the public status after that no-op completes.
    let mut readiness = CausalReadiness {
        minimum_request_seq: 1,
        ..Default::default()
    };
    let mut next_event = async || {
        assert!(
            events.len() < 24,
            "causal witness transition budget exhausted"
        );
        println!("waiting for causal witness; CLI pin={published}; events={events:?}");
        let event = next_causal_event(&witness, deadline).await;
        assert_eq!(event["rootKey"], root_key, "foreign causal witness");
        events.push(event.clone());
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
            peer.assert_success("trellis mcp");
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
        let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
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
        crate::child::assert_cli_success(&output, "index");
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
    peer.assert_success("trellis mcp");
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
      "watchGeneration":generation,"queueEmpty":kind == "WATCH_ACK","requestSeq":0})
}

#[test]
fn causal_readiness_requires_post_request_ack_even_at_unchanged_pin() {
    let stream = uuid::Uuid::new_v4();
    let owner = uuid::Uuid::new_v4();
    let epoch = uuid::Uuid::new_v4();
    let pin = json!({"indexGeneration":uuid::Uuid::new_v4().to_string(),"indexRevision":1});
    let mut state = CausalReadiness {
        minimum_request_seq: 1,
        ..Default::default()
    };
    for (seq, kind) in [(1, "WATCH_PENDING"), (2, "H_READY"), (3, "WATCH_ACK")] {
        state.observe(
            &causal_fixture_event(stream, seq, 1, owner, epoch, kind, &pin, 1),
            &pin,
        );
    }
    assert!(!state.settled(), "startup ACK predates the queued no-op");
    let mut completed = causal_fixture_event(stream, 4, 1, owner, epoch, "WATCH_ACK", &pin, 1);
    completed["requestSeq"] = json!(1);
    state.observe(&completed, &pin);
    assert!(state.settled(), "same-generation post-request ACK settles");
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
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_workers_for_tests(Default::default()),
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
    assert_eq!(envelope["data"]["languages"].as_array().unwrap().len(), 5);
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
    // This test's own worker budget, not the one the binary's other tests share.
    let workers = trellis::mcp::CallWorkersForTests::default();
    let budget = workers.clone();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_deadline_for_tests(Duration::from_millis(300))
                .with_call_workers_for_tests(budget),
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
    // reach the client. Once its worker has exited, the next line on the
    // connection is the next call's reply, not a late result.
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while workers.live() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the timed-out worker did not exit");
    writeln!(client, "{}", modern(json!(62), "tools/list", json!({}))).unwrap();
    let mut next = String::new();
    reader.read_line(&mut next).unwrap();
    let next: Value = serde_json::from_str(&next).unwrap();
    assert_eq!(
        next["id"], 62,
        "late result was written after the deadline reply: {next}"
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
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_workers_for_tests(Default::default()),
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
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_workers_for_tests(Default::default()),
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
    assert_eq!(envelope["data"]["languages"].as_array().unwrap().len(), 5);
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
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_workers_for_tests(Default::default()),
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
        "trellis_workspace_describe: 1 workspace; full result in structuredContent."
    );
    resume_tx.send(()).unwrap();
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn obsolete_cold_index_recovers_over_mcp_without_explicit_options_or_fifo() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function UpgradeTarget() {}\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    let original = store
        .publish_native(
            &graph,
            &capture,
            &native,
            &writer,
            store.index_baseline().unwrap(),
            &cancel,
        )
        .unwrap();
    assert!(store.earliest_unfinished_request().unwrap().is_none());
    drop(writer);
    drop(store);
    // No live SQLite handle remains while changing the stored format marker.
    let db = rusqlite::Connection::open(topology.index_db(&identity)).unwrap();
    db.execute_batch(
        "PRAGMA ignore_check_constraints=ON; UPDATE index_metadata SET schema_version=7;",
    )
    .unwrap();
    drop(db);

    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(1930, &identity).unwrap();
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
    tokio::task::spawn_blocking(move || {
        entered_rx
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap()
    })
    .await
    .unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 1930, None);
    writeln!(client, "{}", modern(json!(1931), "tools/call", json!({
        "name":"trellis_find_symbols", "arguments":{"schemaVersion":1,"query":"UpgradeTarget"}
    }))).unwrap();
    let pending = next_reply(&mut reader);
    tool(&pending, Some("index_not_ready"), true);
    writeln!(
        client,
        "{}",
        modern(
            json!(1932),
            "tools/call",
            json!({
                "name":"trellis_workspace_describe", "arguments":{"schemaVersion":1}
            })
        )
    )
    .unwrap();
    let described = next_reply(&mut reader);
    tool(&described, None, true);
    let pending = &described["result"]["structuredContent"];
    assert_eq!(pending["catchingUp"], true, "{described}");
    assert_eq!(pending["data"]["indexState"], "reconciling", "{described}");
    assert!(pending["evidenceBasis"].is_null());

    let version = runtime.state_version();
    resume_tx.send(()).unwrap();
    let ready_runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        ready_runtime.wait_state_change(version, trellis::testing::HANG_BACKSTOP);
        assert_ne!(
            ready_runtime.state_version(),
            version,
            "startup did not publish a state transition"
        );
        assert!(
            ready_runtime.reconciliation_error().is_none(),
            "{:?}",
            ready_runtime.reconciliation_error()
        );
    })
    .await
    .unwrap();
    writeln!(
        client,
        "{}",
        modern(
            json!(1933),
            "tools/call",
            json!({
                "name":"trellis_workspace_describe", "arguments":{"schemaVersion":1}
            })
        )
    )
    .unwrap();
    let ready = next_reply(&mut reader);
    tool(&ready, None, true);
    let ready = &ready["result"]["structuredContent"];
    assert_eq!(ready["data"]["indexState"], "ready");
    assert_ne!(
        ready["evidenceBasis"]["indexGeneration"],
        original.index_generation.to_string()
    );
    writeln!(client, "{}", modern(json!(1934), "tools/call", json!({
        "name":"trellis_find_symbols", "arguments":{"schemaVersion":1,"query":"UpgradeTarget"}
    }))).unwrap();
    let found = next_reply(&mut reader);
    tool(&found, None, true);
    assert_eq!(
        found["result"]["structuredContent"]["data"]["symbols"][0]["name"],
        "UpgradeTarget"
    );
    drop(reader);
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_workers_for_tests(Default::default()),
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
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_workers_for_tests(Default::default()),
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
        5
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
    let codes = trellis::mcp::catalog::list(true)["tools"][0]["outputSchema"]["$defs"]["Failure"]["properties"]["error"]["properties"]["code"]["enum"].as_array().unwrap().clone();
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
        peer.assert_success("trellis mcp");
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
        // Poison-tolerant: `Drop` opens the gate while a failed test unwinds,
        // and a second panic there would abort the whole test binary.
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .1 = true;
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

/// Wait for the checkout's Architecture build, on the owner's signal: an unscoped read that
/// beats it to the 5 s call deadline answers a retryable failure, on purpose.
pub(crate) async fn architecture_built(
    registry: &std::sync::Arc<tokio::sync::Mutex<trellis::daemon::registry::CheckoutRegistry>>,
    identity: &trellis::store::topology::WorkspaceIdentity,
) {
    let runtime = registry.lock().await.activate(&identity.root_key).unwrap();
    tokio::task::spawn_blocking(move || runtime.await_architecture_for_tests())
        .await
        .unwrap();
}

/// One daemon-socket MCP connection served on a thread.
pub(crate) fn socket_session(
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

/// A [`socket_session`] whose call workers count against `workers`, its
/// test's own budget, never the process-wide one the other tests in this
/// binary share (#869).
fn budgeted_session(
    identity: &trellis::store::topology::WorkspaceIdentity,
    registry: &std::sync::Arc<tokio::sync::Mutex<trellis::daemon::registry::CheckoutRegistry>>,
    session: u64,
    deadline: Option<Duration>,
    workers: &trellis::mcp::CallWorkersForTests,
) -> (
    std::os::unix::net::UnixStream,
    BufReader<std::os::unix::net::UnixStream>,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    served_session(
        identity,
        registry,
        session,
        deadline,
        Default::default(),
        workers.clone(),
    )
}

/// A session with its own call-worker budget, never the process-wide one the
/// other tests in this binary share (#869).
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
    served_session(
        identity,
        registry,
        session,
        deadline,
        hooks,
        Default::default(),
    )
}

fn served_session(
    identity: &trellis::store::topology::WorkspaceIdentity,
    registry: &std::sync::Arc<tokio::sync::Mutex<trellis::daemon::registry::CheckoutRegistry>>,
    session: u64,
    deadline: Option<Duration>,
    hooks: trellis::mcp::TransportHooksForTests,
    workers: trellis::mcp::CallWorkersForTests,
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
    let workspace = workspace
        .with_transport_hooks_for_tests(hooks)
        .with_call_workers_for_tests(workers);
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

pub(crate) fn next_reply(reader: &mut BufReader<std::os::unix::net::UnixStream>) -> Value {
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
    // #528: the refusal names the per-connection limit so an agent can adapt.
    assert_eq!(
        envelope["error"]["message"],
        "Too many concurrent tool calls on this connection (limit 4); wait for one to finish"
    );
    assert_eq!(
        refused["result"]["content"][0]["text"],
        "trellis_workspace_describe failed: too_many_requests: Too many concurrent tool calls on this connection (limit 4); wait for one to finish"
    );
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
    let workers = trellis::mcp::CallWorkersForTests::default();
    let mut connections = Vec::new();
    for connection in 0..4 {
        let (mut client, mut reader, session) = budgeted_session(
            &identity,
            &registry,
            72,
            Some(Duration::from_millis(300)),
            &workers,
        );
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
    assert_eq!(workers.live(), 16);
    // A new connection: every permit is still held by a timed-out worker.
    let (mut client, mut reader, session) =
        budgeted_session(&identity, &registry, 72, None, &workers);
    writeln!(client, "{}", describe_call(99)).unwrap();
    let refused = next_reply(&mut reader);
    assert_eq!(
        refused["result"]["structuredContent"]["error"]["code"], "too_many_requests",
        "{refused}"
    );
    assert_eq!(
        refused["result"]["structuredContent"]["error"]["message"],
        "Too many concurrent tool calls in the daemon (limit 16); retry shortly"
    );
    gate.open();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        while workers.live() != 0 {
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
    let workers = trellis::mcp::CallWorkersForTests::default();
    let (mut client, mut reader, session) =
        budgeted_session(&identity, &registry, 73, None, &workers);
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
        while workers.live() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the cancelled worker did not exit");
    // The worker has exited, so the next line is the next call's reply unless
    // the cancelled call wrote one.
    let tools_list = modern(json!(3), "tools/list", json!({}));
    writeln!(client, "{tools_list}").unwrap();
    let next = next_reply(&mut reader);
    assert_eq!(next["id"], 3, "the cancelled call wrote: {next}");
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
    let (identity, registry, runtime) = ready_registry(temp.path(), 81).await;
    let (send_decision, entered, release) = hold_send_decision(1);
    let (frame_read, read) = frames_read();
    let deadline = Duration::from_millis(300);
    // A paused call clock: call 1 reaches its send decision however loaded the
    // host is, and only the test moves time past either deadline (#869). A
    // past origin makes any accidental real-clock check fail immediately.
    runtime
        .freeze_mcp_call_clock_for_tests(std::time::Instant::now() - trellis::mcp::CALL_DEADLINE);
    let clock = runtime.clone();
    let hooks = trellis::mcp::TransportHooksForTests {
        paused_clock_for_tests: Some(std::sync::Arc::new(move || {
            clock.mcp_call_clock_now_for_tests()
        })),
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
    read.recv_timeout(trellis::testing::HANG_BACKSTOP).unwrap();
    // Pass call 2's own deadline (measured from its read) while held.
    runtime.advance_mcp_clock_for_tests(deadline);
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

/// A find answers wholly from the snapshot it admitted, even while a newer
/// publication is pending; the next call sees the newer revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_find_answers_from_its_admitted_snapshot_while_a_publication_is_pending() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 84).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let worker_gate = ReadGate::default();
    let gate = ReadGate::default();
    let _release_on_failure = McpFillRelease([worker_gate.clone(), gate.clone()]);
    let held = worker_gate.clone();
    let worker_scheduler = scheduler.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let _worker = worker_scheduler.hold_worker_for_tests();
        (held.hook())();
    });
    let held = worker_gate.clone();
    tokio::task::spawn_blocking(move || held.await_held(1))
        .await
        .unwrap();
    let admitted = {
        let (response, _) = runtime.evidence_response().unwrap();
        let revision = response.status().unwrap().revision;
        response.finish(()).unwrap();
        revision
    };
    runtime.set_mcp_admitted_gate_for_tests(gate.hook());
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 84, None);
    let find = |id: u64, query: &str| {
        modern(
            json!(id),
            "tools/call",
            json!({"name":"trellis_find_symbols","arguments":{"schemaVersion":1,"query":query}}),
        )
    };
    writeln!(client, "{}", find(1, "held")).unwrap();
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    // While that read holds its snapshot, change the source so the worker
    // has a newer publication pending.
    fs::write(identity.root.join("a.js"), "function moved() {}\n").unwrap();
    let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
        notify::event::DataChange::Content,
    )))
    .add_path(identity.root.join("a.js"));
    assert!(scheduler.submit_watch_event_for_tests(Ok(event)));
    assert!(scheduler.checkout_watch_pending());
    gate.open();
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(
        envelope["evidenceBasis"]["indexRevision"],
        json!(admitted.index_revision),
        "{reply}"
    );
    let symbols = envelope["data"]["symbols"].as_array().unwrap();
    assert_eq!(symbols.len(), 1, "{reply}");
    assert_eq!(symbols[0]["name"], "held", "{reply}");
    // One snapshot: every coverage row carries the same native revision.
    let coverage = envelope["data"]["coverage"].as_array().unwrap();
    assert!(
        coverage
            .iter()
            .all(|c| c["revisionId"] == coverage[0]["revisionId"]),
        "{reply}"
    );
    worker_gate.open();
    worker.await.unwrap();
    // Once published, a new call answers from the newer revision.
    let newer = tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        for id in 2.. {
            writeln!(client, "{}", find(id, "moved")).unwrap();
            let reply = tokio::task::block_in_place(|| next_reply(&mut reader));
            let envelope = &reply["result"]["structuredContent"];
            if envelope["data"]["symbols"]
                .as_array()
                .is_some_and(|s| s.len() == 1)
            {
                return reply;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        unreachable!()
    })
    .await
    .expect("the edit was never published");
    assert!(
        newer["result"]["structuredContent"]["evidenceBasis"]["indexRevision"]
            .as_u64()
            .unwrap()
            > admitted.index_revision,
        "{newer}"
    );
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
    assert_eq!(
        seventeenth["error"]["message"],
        "Too many outstanding requests on this connection (limit 16)"
    );
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
    // The reader proves exactly 2,000 replies; the flood may stop at that count.
    assert!(sent >= 2_000);
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
        "function Alpha() {}\nfunction Alphabet() {}\nfunction xAlpha() {}\nfunction $Cafe\u{301}() {}\nfunction $Café() {}\nfunction Écho() {}\nfunction écho() {}\nfunction $Find() {}\n".to_owned();
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    let (diagnostics_tx, diagnostics_rx) = mpsc::channel::<String>();
    let hooks = trellis::mcp::TransportHooksForTests {
        evidence_error: Some(std::sync::Arc::new(move |id, stage, detail| {
            let _ = diagnostics_tx.send(format!("{id} {stage}: {detail}"));
        })),
        ..Default::default()
    };
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_transport_hooks_for_tests(hooks)
                .with_call_workers_for_tests(Default::default()),
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
    // 51 matches at limit 50: the first 50, truncated by the limit. With
    // coverage bounded by the emitted symbols (#156), all fifty fit the cap.
    assert_eq!(max_symbols, 50, "{max_limit}");
    assert_eq!(max_limit["result"]["structuredContent"]["truncated"], true);
    assert_eq!(
        max_limit["result"]["structuredContent"]["truncationReason"],
        "limit"
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
    let unicode = ask(&mut client, 153, "$Caf", Some(20), None);
    assert_eq!(
        unicode["result"]["isError"],
        false,
        "selected read details: {:?}; response: {unicode}",
        diagnostics_rx.try_iter().collect::<Vec<_>>()
    );
    tool(&unicode, None, true);
    assert_eq!(
        unicode["result"]["structuredContent"]["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .take(2)
            .map(|symbol| symbol["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["$Cafe\u{301}", "$Café"]
    );
    // Repeat the formerly intermittent selected Unicode read under modest load.
    // A failure stops the loop and reports its captured internal chain.
    for id in 200..220 {
        let repeated = ask(&mut client, id, "$Caf", Some(20), None);
        assert_eq!(
            repeated["result"]["isError"],
            false,
            "request {id} selected read details: {:?}; response: {repeated}",
            diagnostics_rx.try_iter().collect::<Vec<_>>()
        );
    }
    let composed = ask(&mut client, 154, "$Café", Some(20), None);
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
    // #156: a zero-hit find carries no per-document rows; the searched
    // scope is reported as per-language tuple counts from the same snapshot.
    // #715: only the searched languages with selected tuples (Java's are unselected).
    assert_eq!(data["data"]["coverage"], json!([]));
    let row = |language: &str, counts: [u64; 5]| {
        json!({"language":language,"nativeCoverageTier":"syntax","coverage":{
            "selected":counts[0],"complete":counts[1],"partial":counts[2],
            "failed":counts[3],"unselected":counts[4]},
            "semantic":{"available":false,"producerId":null,"freshness":"unavailable",
                "coverage":{"selected":0,"complete":0,"partial":0,"failed":0,"unselected":0},
                "documents":[],
                "note":"no semantic producer published; showing syntax","reimport":null}})
    };
    assert_eq!(
        data["data"]["scopeCoverage"],
        json!([
            row("rust", [1, 1, 0, 0, 0]),
            row("python", [2, 0, 1, 1, 0]),
            row("javascript", [1, 1, 0, 0, 0]),
        ])
    );
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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

/// Publish an index for a fresh checkout with `files`, leaving it inactive.
pub(crate) fn published_inactive_checkout(
    temp: &std::path::Path,
    files: &[(&str, &str)],
) -> (
    trellis::store::topology::WorkspaceIdentity,
    trellis::store::topology::TopologyRoots,
) {
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let root = checkout(temp);
    for (path, text) in files {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp, &root));
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
    let topology = TopologyRoots::isolated_for_tests(temp.join("cache"), temp.join("data"));
    (identity, topology)
}

/// Send one tool call over a daemon socket session and return its reply.
fn one_socket_call(
    identity: trellis::store::topology::WorkspaceIdentity,
    registry: trellis::daemon::registry::CheckoutRegistry,
    session_id: u64,
    deadline: Duration,
    tool_name: &str,
    arguments: Value,
) -> Value {
    use std::os::unix::net::UnixStream;
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, server) = UnixStream::pair().unwrap();
    client
        .set_read_timeout(Some(trellis::testing::HANG_BACKSTOP))
        .unwrap();
    let daemon_handle = tokio::runtime::Handle::current();
    let session = std::thread::spawn(move || {
        trellis::mcp::run_socket(
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_deadline_for_tests(deadline)
                .with_call_workers_for_tests(Default::default()),
            server,
            registry,
            session_id,
            daemon_handle,
        )
    });
    writeln!(
        client,
        "{}",
        modern(
            json!(7),
            "tools/call",
            json!({"name":tool_name,"arguments":arguments})
        )
    )
    .unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    serde_json::from_str(&line).unwrap()
}

/// #156: find's required coverage no longer grows with the repository. A
/// zero-hit query over a 270-document scope, which overflowed the 64 KiB cap
/// when every searched tuple was returned, now answers with scope counts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_find_zero_hit_over_a_large_scope_fits_with_scope_counts() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let files: Vec<(String, String)> = (0..270)
        .map(|i| {
            (
                format!("module_{i:03}.js"),
                format!("function helper_{i:03}() {{ return {i}; }}\n"),
            )
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let (identity, topology) = published_inactive_checkout(temp.path(), &borrowed);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(313, &identity).unwrap();
    let reply = one_socket_call(
        identity,
        registry,
        313,
        trellis::mcp::CALL_DEADLINE,
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"zzzz_no_such_symbol"}),
    );
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["data"]["symbols"], json!([]));
    assert_eq!(envelope["data"]["coverage"], json!([]));
    let scope = envelope["data"]["scopeCoverage"].as_array().unwrap();
    let total: u64 = scope
        .iter()
        .map(|row| {
            row["coverage"]["selected"].as_u64().unwrap()
                + row["coverage"]["unselected"].as_u64().unwrap()
        })
        .sum();
    assert_eq!(total, 270, "{envelope}");
    // #715: only the searched language with selected tuples.
    assert_eq!(scope.len(), 1, "{envelope}");
    assert_eq!(scope[0]["language"], "javascript");
    assert_eq!(scope[0]["coverage"]["complete"], 270, "{envelope}");
    assert_eq!(envelope["partial"], false);
}

/// Contention while a cold checkout proves its prior head is retried as
/// contention until the call deadline, never reported as `store_unavailable`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_cold_prior_head_contention_retries_until_the_deadline() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) =
        published_inactive_checkout(temp.path(), &[("a.js", "function Alpha() {}\n")]);
    // An index journal sidecar makes the cold read-only status check busy.
    let index = topology.index_db(&identity);
    let journal = index.with_file_name(format!(
        "{}-journal",
        index.file_name().unwrap().to_string_lossy()
    ));
    fs::write(&journal, b"").unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(311, &identity).unwrap();
    let busy = registry
        .prior_head_probe(&identity.root_key)
        .unwrap()
        .check()
        .unwrap_err();
    assert!(
        trellis::store::cli_read_retryable_contention(&busy),
        "{busy:#}"
    );
    let reply = one_socket_call(
        identity,
        registry,
        311,
        Duration::from_millis(300),
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"Alpha"}),
    );
    // Still busy at the deadline: the call never admitted a read.
    tool(&reply, Some("deadline_exceeded"), true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(
        envelope["error"]["attemptedWorkspace"]["kind"], "path",
        "{reply}"
    );
    fs::remove_file(&journal).unwrap();
}

/// A deadline reached before read admission answers in the pre-resolution
/// shape (Decision 0009), as the connection's own timer would.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_deadline_before_read_admission_uses_the_pre_resolution_shape() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) =
        published_inactive_checkout(temp.path(), &[("a.js", "function Alpha() {}\n")]);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(312, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    // Before the read is admitted, move the call's clock past its deadline.
    let advanced = runtime.clone();
    runtime.set_mcp_read_hook_for_tests(move || {
        advanced.advance_mcp_clock_for_tests(Duration::from_secs(60));
    });
    let reply = one_socket_call(
        identity,
        registry,
        312,
        trellis::mcp::CALL_DEADLINE,
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"Alpha"}),
    );
    tool(&reply, Some("deadline_exceeded"), true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(
        envelope["error"]["attemptedWorkspace"]["kind"], "path",
        "{reply}"
    );
    assert!(envelope.get("workspace").is_none(), "{reply}");
    assert!(envelope.get("catchingUp").is_none(), "{reply}");
}

/// #156, relaxed by #261: find validates the coverage tuple of each hydrated
/// hit only. A corrupt row in a document no hit reaches no longer fails a
/// zero-hit query or a hit elsewhere; a hit on the corrupt document fails as
/// store_unavailable, never a successful answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_find_corrupt_scope_row_fails_only_when_hydrated() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    for query in ["no_such_name", "Alpha", "Beta"] {
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
        fs::write(root.join("b.js"), "function Beta() {}\n").unwrap();
        let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
        assert!(
            response
                .mcp_find("no_such_name", 20, None)
                .unwrap()
                .symbols
                .is_empty()
        );
        assert_eq!(
            response.mcp_find("Alpha", 20, None).unwrap().symbols.len(),
            1
        );
        response.finish(()).unwrap();
        drop(response);
        let identity = WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .attach_marker()
            .unwrap();
        let topology =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let mut registry = CheckoutRegistry::with_roots(topology);
        registry.attach_launch(2621, &identity).unwrap();
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
        let scheduler = runtime.browser_scheduler().unwrap();
        let _worker = scheduler.hold_worker_for_tests();
        // Only b.js's tuple becomes inconsistent (unselected yet "failed").
        let writer = store.publication_for_tests().unwrap();
        store
            .mcp_set_coverage_for_tests(&writer, "b.js", false, "failed")
            .unwrap();
        drop(writer);
        let response = store.evidence_response().unwrap();
        if query != "Beta" {
            let found = response
                .mcp_find(query, 20, None)
                .unwrap_or_else(|error| panic!("{query}: {error:#}"));
            assert_eq!(
                found.symbols.len(),
                usize::from(query == "Alpha"),
                "{query}"
            );
            assert_eq!(found.scope[3], [1, 1, 0, 0, 1], "{query}");
            response.finish(()).unwrap();
            continue;
        }
        let error = response.mcp_find(query, 20, None).map(|_| ()).unwrap_err();
        let reason = format!("{error:#}");
        assert!(
            reason.contains("store_unavailable: inconsistent native coverage tuple"),
            "{query}: {reason}"
        );
        drop(response);
        let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
        let (mut client, mut reader, session) = socket_session(&identity, &registry, 2621, None);
        writeln!(
            client,
            "{}",
            modern(
                json!(1),
                "tools/call",
                json!({
                    "name":"trellis_find_symbols", "arguments":{"schemaVersion":1,"query":query}
                })
            )
        )
        .unwrap();
        let reply = next_reply(&mut reader);
        tool(&reply, Some("store_unavailable"), true);
        let envelope = &reply["result"]["structuredContent"];
        assert!(envelope.get("data").is_none(), "{query}: {reply}");
        assert!(
            envelope["error"]["currentBasis"].is_null(),
            "{query}: {reply}"
        );
        assert!(
            runtime.evidence_response().is_err(),
            "tool boundary did not latch recovery: {query}"
        );
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_find_required_rows_and_coverage_fail_closed() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    // A well-formed but wrong content hash is trusted on read (#261).
    for fault in ["invalid_coverage", "missing_scope", "missing_header"] {
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
        let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp, &linked));
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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

    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
        "function Alpha(first, second) {\r\n return first;\r\n}\r\n",
    )
    .unwrap();
    fs::write(root.join("empty.js"), "").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
            trellis::mcp::OpenedWorkspace::new(identity)
                .with_call_workers_for_tests(Default::default()),
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
        "trellis_declaration",
        json!({"schemaVersion":1,"symbolId":id}),
    );
    tool(&inspected, None, true);
    let envelope = &inspected["result"]["structuredContent"];
    assert_eq!(
        envelope["catchingUp"], true,
        "validated prior head during H"
    );
    assert_eq!(envelope["data"]["declaration"]["syntaxId"], id);
    assert_eq!(envelope["data"]["displayKey"], "a.js#Alpha");
    let declaration_data = &envelope["data"];
    for field in ["document", "revisionId", "provenanceId"] {
        assert!(declaration_data["declaration"].get(field).is_none());
    }
    assert!(declaration_data.get("contentHash").is_none());
    let version = emitted_document(declaration_data, declaration_data);
    let proof = &declaration_data["provenance"][0];
    assert_eq!(version["document"], proof["document"]);
    assert_eq!(version["revisionId"], proof["revisionId"]);
    assert_eq!(version["contentHash"], proof["contentHash"]);
    assert_eq!(version["provenanceId"], proof["id"]);
    assert_eq!(declaration_data["lines"], json!({"start":1,"end":3}));
    assert_eq!(symbol["lines"], json!({"start":1,"end":3}));
    assert_eq!(
        emitted_document(&envelope["data"], &envelope["data"])["contentHash"],
        emitted_document(&found["result"]["structuredContent"]["data"], symbol)["contentHash"]
    );
    assert_eq!(envelope["data"]["coverage"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["data"]["provenance"].as_array().unwrap().len(), 1);
    let found_envelope = &found["result"]["structuredContent"];
    for (request_id, source_envelope, item) in [
        (5, found_envelope, symbol),
        (6, envelope, &envelope["data"]),
    ] {
        let source_data = &source_envelope["data"];
        let entry = emitted_document(source_data, item);
        let selected = ask(
            &mut client,
            request_id,
            "trellis_read_source",
            json!({
                "schemaVersion":1,"path":entry["document"]["path"],
                "startLine":item["lines"]["start"],"endLine":item["lines"]["end"],
                "expectedBasis":source_envelope["evidenceBasis"],
                "expectedContentHash":entry["contentHash"]
            }),
        );
        tool(&selected, None, true);
        let selected_range = &selected["result"]["structuredContent"]["data"]["byteRange"];
        let range = if request_id == 5 {
            &item["range"]
        } else {
            &item["declaration"]["range"]
        };
        assert!(selected_range["start"].as_u64().unwrap() <= range["start"].as_u64().unwrap());
        assert!(range["end"].as_u64().unwrap() <= selected_range["end"].as_u64().unwrap());
    }
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
    let empty_module =
        found["result"]["structuredContent"]["data"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|symbol| {
                emitted_document(&found["result"]["structuredContent"]["data"], symbol)["document"]
                    ["path"]
                    == "empty.js"
            })
            .expect("empty-file module declaration");
    assert_eq!(empty_module["range"], json!({"start":0,"end":0}));
    assert!(empty_module["lines"].is_null());
    let empty_declaration = ask(
        &mut client,
        7,
        "trellis_declaration",
        json!({"schemaVersion":1,"symbolId":empty_module["symbolId"]}),
    );
    tool(&empty_declaration, None, true);
    assert!(empty_declaration["result"]["structuredContent"]["data"]["lines"].is_null());
    let empty_version = emitted_document(
        &empty_declaration["result"]["structuredContent"]["data"],
        &empty_declaration["result"]["structuredContent"]["data"],
    );
    let no_line = ask(
        &mut client,
        8,
        "trellis_read_source",
        json!({
            "schemaVersion":1,"path":"empty.js","startLine":1,"endLine":1,
            "expectedBasis":empty_declaration["result"]["structuredContent"]["evidenceBasis"],
            "expectedContentHash":empty_version["contentHash"]
        }),
    );
    tool(&no_line, Some("range_too_large"), true);
    let unnamed = found["result"]["structuredContent"]["data"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"].is_null() && emitted_document(&found["result"]["structuredContent"]["data"], s)["document"]["path"] == "a.js")
        .expect("selected unnamed module");
    let inspected = ask(
        &mut client,
        4,
        "trellis_declaration",
        json!({
            "schemaVersion":1,"symbolId":unnamed["symbolId"]
        }),
    );
    tool(&inspected, None, true);
    let module_data = &inspected["result"]["structuredContent"]["data"];
    assert_eq!(module_data["lines"], json!({"start":1,"end":3}));
    assert_eq!(unnamed["lines"], module_data["lines"]);
    let version = emitted_document(module_data, module_data);
    let complete = ask(
        &mut client,
        9,
        "trellis_read_source",
        json!({
            "schemaVersion":1,"path":"a.js","startLine":1,"endLine":3,
            "expectedBasis":inspected["result"]["structuredContent"]["evidenceBasis"],
            "expectedContentHash":version["contentHash"]
        }),
    );
    tool(&complete, None, true);
    let selected_end = complete["result"]["structuredContent"]["data"]["byteRange"]["end"]
        .as_u64()
        .unwrap();
    assert_eq!(
        selected_end,
        module_data["declaration"]["range"]["end"].as_u64().unwrap()
    );
    assert_eq!(
        selected_end as usize,
        fs::read(root.join("a.js")).unwrap().len()
    );
    assert_eq!(
        inspected["result"]["structuredContent"]["data"]["displayKey"],
        format!(
            "a.js#{}@{}",
            unnamed["kind"].as_str().unwrap(),
            unnamed["range"]["start"].as_u64().unwrap()
        )
    );
    // All pinned W5 reads above must use the validated prior head while H is held.
    // Releasing startup reconciliation earlier may publish a new basis mid-test.
    release_tx.send(()).unwrap();
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    // A missing header is corruption; a well-formed but wrong content hash
    // is trusted on read (#261).
    {
        let fault = "missing_header";
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        fs::write(root.join("a.js"), "function Alpha() {}\n").unwrap();
        let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
                "trellis_declaration",
                Some(&json!({
                    "schemaVersion":1,"symbolId":invalid
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
    let name = format!("Alpha{}", "a".repeat(20_000));
    fs::write(root.join("a.js"), format!("function {name}() {{}}\n")).unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    writeln!(
        client,
        "{}",
        modern(
            json!(1),
            "tools/call",
            json!({
                "name":"trellis_declaration", "arguments":{"schemaVersion":1,"symbolId":id}
            })
        )
    )
    .unwrap();
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
        "name":"trellis_declaration", "arguments":{"schemaVersion":1,
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
        "trellis_outgoing_calls",
        json!({"schemaVersion":1,"symbolId":symbol_id}),
    );
    tool(&response, None, true);
    let data = &response["result"]["structuredContent"]["data"];
    assert_eq!(response["result"]["structuredContent"]["catchingUp"], true);
    assert_eq!(data["calls"].as_array().unwrap().len(), 1);
    assert_eq!(data["calls"][0]["calleeText"], "Beta");
    let call = &data["calls"][0];
    let document = emitted_document(data, call);
    let lines = &call["lines"];
    assert!(lines["start"].as_u64().unwrap() <= lines["end"].as_u64().unwrap());
    let selected_source = ask(
        &mut client,
        &mut reader,
        4,
        "trellis_read_source",
        json!({
            "schemaVersion":1,"path":document["document"]["path"],
            "startLine":lines["start"],"endLine":lines["end"],
            "expectedBasis":response["result"]["structuredContent"]["evidenceBasis"],
            "expectedContentHash":document["contentHash"]
        }),
    );
    tool(&selected_source, None, true);
    let returned = &selected_source["result"]["structuredContent"]["data"]["byteRange"];
    assert!(returned["start"].as_u64().unwrap() <= call["range"]["start"].as_u64().unwrap());
    assert!(returned["end"].as_u64().unwrap() >= call["range"]["end"].as_u64().unwrap());
    assert_eq!(data["calls"][0]["disposition"], "unresolved");
    assert!(data["calls"][0]["binding"].is_null());
    let mut stale = found["result"]["structuredContent"]["evidenceBasis"].clone();
    stale["indexRevision"] = json!(stale["indexRevision"].as_u64().unwrap() + 1);
    let refused = ask(
        &mut client,
        &mut reader,
        3,
        "trellis_outgoing_calls",
        json!({"schemaVersion":1,"symbolId":format!("sid:v1:{}","0".repeat(32)),"expectedBasis":stale}),
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
    mcp_outgoing_wire_fixture(files, owner, limit, advance_hints, false, false).0
}

fn mcp_outgoing_wire_fixture(
    files: &[(&str, &str)],
    owner: &str,
    limit: Option<usize>,
    advance_hints: Option<Vec<(&'static str, u64)>>,
    legacy: bool,
    check_selected_after_hints: bool,
) -> (Value, Vec<u8>) {
    mcp_outgoing_wire_fixture_delayed(
        files,
        owner,
        limit,
        advance_hints,
        legacy,
        check_selected_after_hints,
        OutgoingFixtureTiming::default(),
    )
}

#[derive(Default)]
struct OutgoingFixtureTiming {
    accept_delay: Duration,
    call_deadline: Option<Duration>,
    paused_call_clock: bool,
}

/// As `mcp_outgoing_wire_fixture`, sending the call `accept_delay` after the
/// hint clock is frozen, so a test can show the hint budget does not depend on
/// acceptance timing (#131).
fn mcp_outgoing_wire_fixture_delayed(
    files: &[(&str, &str)],
    owner: &str,
    limit: Option<usize>,
    advance_hints: Option<Vec<(&'static str, u64)>>,
    legacy: bool,
    check_selected_after_hints: bool,
    timing: OutgoingFixtureTiming,
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    if check_selected_after_hints {
        // The virgin fixture can briefly request normal startup reconciliation.
        // Prove a clean admitted head before attributing recovery to optional work.
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
                    loop {
                        if let Ok((selected, catching_up)) = runtime.evidence_response()
                            && selected.finish(()).is_ok()
                            && !catching_up
                            && !runtime.mcp_recovery_pending_for_tests().unwrap()
                        {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .expect("outgoing fixture did not settle its clean head");
            });
        });
    }
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let mut hooks = if timing.paused_call_clock {
        // A past origin makes any accidental real-clock check fail immediately.
        runtime.freeze_mcp_call_clock_for_tests(
            std::time::Instant::now() - trellis::mcp::CALL_DEADLINE,
        );
        let clock = runtime.clone();
        trellis::mcp::TransportHooksForTests {
            paused_clock_for_tests: Some(std::sync::Arc::new(move || {
                clock.mcp_call_clock_now_for_tests()
            })),
            ..Default::default()
        }
    } else {
        trellis::mcp::TransportHooksForTests::default()
    };
    // The hint stages are this connection's own barrier, never process
    // environment that the other tests in this binary read (#869).
    let hint_barrier = advance_hints.map(|stages| {
        let path = temp.path().join("hint.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(false).unwrap();
        let barrier = trellis::testing::PhaseBarrier::new(stages[0].0, &path);
        hooks.scope.phase = Some(barrier.clone());
        (stages, listener, barrier)
    });
    let (mut client, mut reader, session) =
        hooked_session(&identity, &registry, 301, timing.call_deadline, hooks);
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
    let mut args = json!({"schemaVersion":1,"symbolId":symbol["symbolId"]});
    if let Some(limit) = limit {
        args["limit"] = json!(limit);
    }
    if check_selected_after_hints {
        assert!(
            !runtime.mcp_recovery_pending_for_tests().unwrap(),
            "fixture starts with pending recovery"
        );
    }
    runtime.freeze_mcp_hint_clock_for_tests();
    let runtime_after_hints = runtime.clone();
    let budget_gate = if let Some((stages, listener, barrier)) = hint_barrier {
        Some(std::thread::spawn(move || {
            for (index, (stage, millis)) in stages.iter().enumerate() {
                let mut gate = trellis::testing::accept_within_backstop(&listener);
                let mut received = String::new();
                BufReader::new(gate.try_clone().unwrap())
                    .read_line(&mut received)
                    .unwrap();
                assert_eq!(received, format!("{stage}\n"));
                runtime.advance_mcp_clock_for_tests(Duration::from_millis(*millis));
                if let Some((next_stage, _)) = stages.get(index + 1) {
                    barrier.set_stages(next_stage);
                }
                gate.write_all(b"!").unwrap();
            }
        }))
    } else {
        None
    };
    if !timing.accept_delay.is_zero() {
        std::thread::sleep(timing.accept_delay);
    }
    writeln!(
        client,
        "{}",
        call(legacy, json!(2), "trellis_outgoing_calls", args)
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
    }
    if check_selected_after_hints {
        assert!(
            !runtime_after_hints
                .mcp_recovery_pending_for_tests()
                .unwrap(),
            "optional expiry must not mark recovery"
        );
        let (selected, _) = runtime_after_hints.evidence_response().unwrap();
        let found = selected.mcp_find(owner, 20, None).unwrap();
        assert!(
            found
                .symbols
                .iter()
                .any(|symbol| symbol.declaration.name.as_deref() == Some(owner))
        );
        selected.finish(()).unwrap();
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    (response, frame)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_corrupt_find_declaration_outgoing_omit_current_basis_but_stale_pin_conflicts() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"), "function Alpha() { Alpha(); }\n").unwrap();
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    let id = selected.mcp_find("Alpha", 20, None).unwrap().symbols[0]
        .declaration
        .syntax_id
        .clone();
    selected.finish(()).unwrap();
    drop(selected);
    drop(store);
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(371, &identity).unwrap();
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
    .expect("runtime did not complete startup");
    let scheduler = runtime.browser_scheduler().unwrap();
    let _worker = scheduler.hold_worker_for_tests();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    for corrupt in [false, true] {
        if corrupt {
            let store =
                trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
            let writer = store.publication_for_tests().unwrap();
            store
                .mcp_corrupt_find_for_tests(&writer, "missing_header")
                .unwrap();
            drop(writer);
            drop(store);
        }
        for legacy in [false, true] {
            let (mut client, mut reader, session) = socket_session(&identity, &registry, 371, None);
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
            for (name, base) in [
                (
                    "trellis_find_symbols",
                    json!({"schemaVersion":1,"query":"Alpha"}),
                ),
                (
                    "trellis_declaration",
                    json!({"schemaVersion":1,"symbolId":id}),
                ),
                (
                    "trellis_outgoing_calls",
                    json!({"schemaVersion":1,"symbolId":id}),
                ),
            ] {
                let mut args = base;
                if !corrupt {
                    args["expectedBasis"] = json!({"indexGeneration":"123e4567-e89b-12d3-a456-426614174000","indexRevision":1});
                }
                writeln!(client, "{}", call(legacy, json!(1), name, args)).unwrap();
                let reply = next_reply(&mut reader);
                tool(
                    &reply,
                    Some(if corrupt {
                        "store_unavailable"
                    } else {
                        "revision_conflict"
                    }),
                    !legacy,
                );
                let envelope = &reply["result"]["structuredContent"];
                if corrupt {
                    assert!(
                        envelope["error"]["currentBasis"].is_null(),
                        "{legacy} {name}: {reply}"
                    );
                    assert!(envelope.get("data").is_none(), "{reply}");
                } else {
                    assert!(
                        envelope["error"]["currentBasis"].is_object(),
                        "{legacy} {name}: {reply}"
                    );
                }
            }
            drop(reader);
            drop(client);
            session.join().unwrap().unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_wire_valid_basis_and_uint_are_not_silently_dropped() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::{
        Store,
        topology::{TopologyRoots, WorkspaceIdentity},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(root.join("a.js"),
        "function Target0() {}\nfunction Target1() {}\nfunction Target2() {}\nfunction Owner() { Target0(); Target1(); Target2(); }\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    registry.attach_launch(370, &identity).unwrap();
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
    .expect("runtime did not complete startup");
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    for legacy in [false, true] {
        let (mut client, mut reader, session) = socket_session(&identity, &registry, 370, None);
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
        let mut ask = |name: &str, args: Value| -> Value {
            writeln!(client, "{}", call(legacy, json!(1), name, args)).unwrap();
            let reply = next_reply(&mut reader);
            assert_eq!(reply["id"], 1, "{reply}");
            reply["result"]["structuredContent"].clone()
        };
        let found = ask(
            "trellis_find_symbols",
            json!({"schemaVersion":1,"query":"Owner"}),
        );
        let pin = found["evidenceBasis"].clone();
        let id = found["data"]["symbols"][0]["symbolId"].clone();
        let requests = [
            (
                "trellis_find_symbols",
                json!({"schemaVersion":1,"query":"Target"}),
            ),
            (
                "trellis_declaration",
                json!({"schemaVersion":1,"symbolId":id}),
            ),
            (
                "trellis_outgoing_calls",
                json!({"schemaVersion":1,"symbolId":id}),
            ),
            (
                "trellis_read_source",
                json!({"schemaVersion":1,"path":"a.js","startLine":1,"endLine":1}),
            ),
        ];
        for (name, base) in requests {
            for stale in [
                json!({"indexGeneration":"123e4567-e89b-12d3-a456-426614174000","indexRevision":1}),
                serde_json::from_str::<Value>(&format!(
                    r#"{{"indexGeneration":"{}","indexRevision":{}.0}}"#,
                    pin["indexGeneration"].as_str().unwrap(),
                    pin["indexRevision"].as_u64().unwrap() + 1
                ))
                .unwrap(),
            ] {
                let mut args = base.clone();
                args["expectedBasis"] = stale.clone();
                let envelope = ask(name, args);
                assert_eq!(
                    envelope["error"]["code"], "revision_conflict",
                    "{legacy} {name} stale={stale}: {envelope}"
                );
                assert_eq!(envelope["error"]["currentBasis"], pin);
            }
            let mut args = base;
            args["expectedBasis"] = serde_json::from_str(&format!(
                r#"{{"indexGeneration":"{}","indexRevision":{}e0}}"#,
                pin["indexGeneration"].as_str().unwrap(),
                pin["indexRevision"].as_u64().unwrap()
            ))
            .unwrap();
            let envelope = ask(name, args);
            assert!(
                envelope.get("data").is_some(),
                "{legacy} {name}: {envelope}"
            );
        }
        for (name, args, expected_count) in [
            (
                "trellis_find_symbols",
                serde_json::from_str::<Value>(
                    r#"{"schemaVersion":1,"query":"Target","limit":1.0}"#,
                )
                .unwrap(),
                1,
            ),
            (
                "trellis_outgoing_calls",
                serde_json::from_str::<Value>(&format!(
                    r#"{{"schemaVersion":1,"symbolId":"{}","limit":2e0}}"#,
                    id.as_str().unwrap()
                ))
                .unwrap(),
                2,
            ),
        ] {
            let envelope = ask(name, args);
            let items = if name == "trellis_find_symbols" {
                &envelope["data"]["symbols"]
            } else {
                &envelope["data"]["calls"]
            };
            assert_eq!(
                items.as_array().map(Vec::len),
                Some(expected_count),
                "{legacy}: {envelope}"
            );
        }
        let lines = serde_json::from_str::<Value>(
            r#"{"schemaVersion":1,"path":"a.js","startLine":1.0,"endLine":10e-1}"#,
        )
        .unwrap();
        let envelope = ask("trellis_read_source", lines);
        assert_eq!(
            envelope["data"]["text"], "function Target0() {}\n",
            "{legacy}: {envelope}"
        );
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_measured_escaped_identifier_matches_decoded_declaration() {
    let reply = mcp_outgoing_fixture(
        &[(
            "a.js",
            "function Target() {}\nfunction Owner() { obj.\\u{54}arget(); }\n",
        )],
        "Owner",
        None,
    );
    tool(&reply, None, true);
    let call = &reply["result"]["structuredContent"]["data"]["calls"][0];
    assert_eq!(call["calleeText"], "\\u{54}arget");
    assert_eq!(
        call["nativeCandidates"]["items"].as_array().unwrap().len(),
        1,
        "{reply}"
    );
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
    // The eight-hint cap is reported per call, never as top-level truncation (#167).
    assert_eq!(envelope["truncated"], false);
    assert_eq!(envelope["truncationReason"], Value::Null);
    assert_eq!(envelope["partial"], true);
    assert_eq!(data["coverage"].as_array().unwrap().len(), 9);
    assert_eq!(data["provenance"].as_array().unwrap().len(), 9);
    assert_eq!(
        emitted_document(data, &hints[0])["document"]["path"],
        "b0.js"
    );
    assert_eq!(
        emitted_document(data, &hints[7])["document"]["path"],
        "b7.js"
    );
    assert!(hints.iter().all(
        |candidate| emitted_document(data, candidate)["document"]["language"] == "javascript"
    ));
    let documents = data["documents"].as_array().unwrap();
    assert_eq!(documents.len(), 9); // One owner version and eight emitted hint versions.
    for candidate in hints {
        assert!(candidate.get("lines").is_none());
        let entry = emitted_document(data, candidate);
        assert!(data["provenance"].as_array().unwrap().iter().any(|proof| {
            proof["id"] == entry["provenanceId"]
                && proof["document"] == entry["document"]
                && proof["revisionId"] == entry["revisionId"]
        }));
    }
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
    // Byte ordering is independent of elapsed time; deadline tests use real clocks.
    let reply = mcp_outgoing_wire_fixture_delayed(
        &borrowed,
        "Owner",
        Some(50),
        None,
        false,
        false,
        OutgoingFixtureTiming {
            paused_call_clock: true,
            ..Default::default()
        },
    )
    .0;
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    let calls = envelope["data"]["calls"].as_array().unwrap();
    assert!(!calls.is_empty() && calls.len() <= 50);
    if calls.len() == 50 {
        assert!(
            calls.iter().any(|call| {
                call["nativeCandidates"]["omitted"]
                    .as_u64()
                    .is_some_and(|n| n > 0)
            }),
            "when all calls fit, optional candidates consume the remaining byte budget"
        );
    }
    // Only calls cut for bytes truncate; clipped hints stay per call (#167).
    let expected_reason = if calls.len() < 50 {
        json!("response_bytes")
    } else {
        Value::Null
    };
    assert_eq!(envelope["truncationReason"], expected_reason);
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
            std::iter::once(
                emitted_document(closure, call)["provenanceId"]
                    .as_str()
                    .unwrap(),
            )
            .chain(
                call["nativeCandidates"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|c| {
                        emitted_document(closure, c)["provenanceId"]
                            .as_str()
                            .unwrap()
                    }),
            )
        })
        .collect();
    assert_eq!(proofs, required);
    assert!(serde_json::to_vec(&reply).unwrap().len() < 65_536);
}

/// #131: the same near-deadline budget, with the call accepted 300 ms after
/// the hint clock was frozen. The hint budget is anchored to acceptance, so
/// the delay cannot move the cap and leak an optional hint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_near_call_deadline_is_independent_of_acceptance_delay() {
    mcp_outgoing_near_call_deadline_body(Duration::from_millis(300)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_near_call_deadline_preserves_required_call_without_optional_work() {
    mcp_outgoing_near_call_deadline_body(Duration::ZERO).await;
}

async fn mcp_outgoing_near_call_deadline_body(accept_delay: Duration) {
    // Required calls finish before the hint budget starts. The row gate advances
    // the virtual clock into the reserved send-decision window, without crossing
    // the overall deadline. Only optional hints may be abandoned.
    // mcp_outgoing_fixture_with_budget, with the call accepted after `accept_delay`.
    let (reply, _) = mcp_outgoing_wire_fixture_delayed(
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
        false,
        false,
        OutgoingFixtureTiming {
            accept_delay,
            ..Default::default()
        },
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
async fn mcp_outgoing_mid_verification_expiry_keeps_required_calls_and_prior_hints() {
    // `Local` and `Target` live in their own versions, so each is verified
    // only inside optional candidate verification. local.js's verification
    // advances nothing and its hint survives; target.js's verification advances
    // the hint clock past the budget, so Target's calls lose their hints while
    // every required call survives.
    let (reply, _) = mcp_outgoing_wire_fixture(
        &[
            (
                "owner.js",
                "function Owner() { Local(); (obj.Unknown)(); Target(); Target(); }\n",
            ),
            ("local.js", "function Local() {}\n"),
            ("target.js", "function Target() {}\n"),
        ],
        "Owner",
        None,
        // Each candidate version is verified once: local.js at no cost, then
        // target.js expires.
        Some(vec![
            ("outgoing_hint_verify_version", 0),
            ("outgoing_hint_verify_version", 1001),
        ]),
        false,
        true,
    );
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    let calls = envelope["data"]["calls"].as_array().unwrap();
    assert_eq!(calls.len(), 4, "mandatory call list survives: {reply}");
    assert_eq!(calls[0]["calleeText"], "Local", "{reply}");
    let local = calls[0]["nativeCandidates"]["items"].as_array().unwrap();
    assert_eq!(local.len(), 1, "the earlier hint survives: {reply}");
    assert_eq!(local[0]["name"], "Local", "{reply}");
    assert_eq!(
        emitted_document(&reply["result"]["structuredContent"]["data"], &local[0])["document"]["path"],
        "local.js",
        "{reply}"
    );
    assert_eq!(calls[0]["nativeCandidates"]["omitted"], 0, "{reply}");
    assert!(calls[1]["calleeRange"].is_null(), "{reply}");
    assert_eq!(calls[1]["nativeCandidates"]["items"], json!([]));
    assert_eq!(calls[1]["nativeCandidates"]["omitted"], 0);
    for call in &calls[2..] {
        assert_eq!(call["calleeText"], "Target", "{reply}");
        assert_eq!(call["nativeCandidates"]["items"], json!([]));
        assert!(call["nativeCandidates"]["omitted"].is_null(), "{reply}");
    }
    assert_eq!(envelope["error"], Value::Null, "{reply}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_verifies_only_the_eight_top_ranked_hints() {
    // Twelve eligible matches in twelve documents. The gate releases exactly
    // eight version verifications; a ninth attempt fails its gate connection.
    // Rank-first verification (#170) never makes that ninth attempt, so the
    // eight top-ranked hints are shown and the other four are unexamined.
    let targets: Vec<(String, String)> = (0..12)
        .map(|i| (format!("b{i:02}.js"), "function Target() {}\n".to_owned()))
        .collect();
    let mut files: Vec<(&str, &str)> = targets
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    files.push(("a.js", "function Owner() { Target(); }\n"));
    let reply = mcp_outgoing_fixture_with_budget(
        &files,
        "Owner",
        None,
        Some(vec![("outgoing_hint_verify_version", 0); 8]),
    );
    tool(&reply, None, true);
    let data = &reply["result"]["structuredContent"]["data"];
    let hints = &data["calls"][0]["nativeCandidates"];
    let shown: Vec<_> = hints["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hint| emitted_document(data, hint)["document"]["path"].clone())
        .collect();
    assert_eq!(
        shown,
        (0..8)
            .map(|i| json!(format!("b{i:02}.js")))
            .collect::<Vec<_>>(),
        "{reply}"
    );
    assert_eq!(hints["omitted"], 4, "{reply}");
    assert_eq!(reply["result"]["structuredContent"]["truncated"], false);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_outgoing_same_version_attested_once_for_repeated_calls() {
    // This gate has one release. A second verification attempt fails its gate
    // connection, so both candidates can survive only with per-version reuse.
    let reply = mcp_outgoing_fixture_with_budget(
        &[(
            "a.js",
            "function Target() {}\nfunction Owner() { Target(); Target(); }\n",
        )],
        "Owner",
        None,
        Some(vec![("outgoing_hint_verify_version", 0)]),
    );
    tool(&reply, None, true);
    let calls = reply["result"]["structuredContent"]["data"]["calls"]
        .as_array()
        .unwrap();
    assert_eq!(calls.len(), 2, "{reply}");
    for call in calls {
        assert_eq!(
            call["nativeCandidates"]["items"].as_array().unwrap().len(),
            1,
            "{reply}"
        );
        assert_eq!(call["nativeCandidates"]["omitted"], 0, "{reply}");
    }
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
    for _ in 0..40 {
        source.push_str("  Target();\n");
    }
    source.push_str("}\n");
    files.push(("owner.js".into(), source));
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, s)| (p.as_str(), s.as_str()))
        .collect();
    for legacy in [false, true] {
        // Framing and candidate ordering are the subject here; the separate
        // deadline tests retain the production five-second call limit.
        let (reply, frame) = mcp_outgoing_wire_fixture_delayed(
            &borrowed,
            "Owner",
            Some(50),
            None,
            legacy,
            false,
            OutgoingFixtureTiming {
                paused_call_clock: true,
                ..Default::default()
            },
        );
        tool(&reply, None, !legacy);
        assert_eq!(frame.last(), Some(&b'\n'));
        assert!(
            frame.len() <= 65_536,
            "complete structured-plus-summary frame exceeds cap: {}",
            frame.len()
        );
        assert_eq!(serde_json::from_slice::<Value>(&frame).unwrap(), reply);
        let envelope = &reply["result"]["structuredContent"];
        let calls = envelope["data"]["calls"].as_array().unwrap();
        assert_eq!(
            calls.len(),
            40,
            "all required calls must fit before hints: {legacy} {}",
            frame.len()
        );
        // Every call fits; hints clipped for bytes are reported per call only (#167).
        assert_eq!(
            envelope["truncationReason"],
            Value::Null,
            "{legacy} {}",
            frame.len()
        );
        assert_eq!(envelope["truncated"], false);
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
                std::iter::once(
                    emitted_document(&envelope["data"], call)["provenanceId"]
                        .as_str()
                        .unwrap(),
                )
                .chain(
                    call["nativeCandidates"]["items"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|item| {
                            emitted_document(&envelope["data"], item)["provenanceId"]
                                .as_str()
                                .unwrap()
                        }),
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
                // Keep the BLOB decodable for the optional ranking query; the
                // stale keyed header then makes the candidate fail verification.
                "UPDATE native_version_declarations SET syntax_id=X'00000000000000000000000000000000' WHERE name='Target'",
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
            "missing_required_owner_header" => (
                "DELETE FROM native_version_headers WHERE syntax_id IN (SELECT syntax_id FROM native_version_declarations WHERE name='Owner')",
                1,
            ),
            "missing_required_owner_coverage" => (
                "UPDATE revision_documents SET coverage_selected=0,coverage_state='omitted',coverage_diagnostic='fixture unselected' WHERE path='owner.js'",
                1,
            ),
            "invalid_required_call_id" => (
                "UPDATE native_version_calls SET id=X'00' WHERE owner_syntax_id IN (SELECT syntax_id FROM native_version_declarations WHERE name='Owner')",
                1,
            ),
            _ => panic!("unknown fixture fault: {fault}"),
        };
        assert_eq!(db.execute(sql, []).unwrap(), expect, "{fault}");
    }
    let mut registry = CheckoutRegistry::with_roots(topology);
    // The subject is this one read's handling of the corrupt rows. An eagerly built
    // Architecture core (#798) reads the same rows at activation, reports the corruption
    // and latches the head for rebuild; a read that lands after it is then refused as
    // store_unavailable whatever its own optional handling (#878). Build only on demand.
    registry.architecture_on_demand_for_tests();
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
            "trellis_outgoing_calls",
            json!({
                "schemaVersion":1,"symbolId":owner_id
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
            std::time::Instant::now() + trellis::testing::HANG_BACKSTOP,
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
            "trellis_outgoing_calls",
            json!({
                "schemaVersion":1,"symbolId":owner_id
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    // The head's eager Architecture build (#798) reads every document: it has finished before
    // the bytes are damaged, so only the reads below see them.
    runtime
        .settled_for_tests(None)
        .await
        .unwrap_or_else(|error| panic!("{error}"));
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
    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
    for fault in ["missing_scope", "bad_digest", "bad_length"] {
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        fs::write(root.join("a.js"), "a\n").unwrap();
        let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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
        let scheduler = runtime.browser_scheduler().unwrap();
        let _worker = scheduler.hold_worker_for_tests();
        let writer = store.publication_for_tests().unwrap();
        store.mcp_corrupt_find_for_tests(&writer, fault).unwrap();
        drop(writer);
        let selected = store.evidence_response().unwrap();
        let error = selected.mcp_source("a.js", 1, 1, None, None).err().unwrap();
        let reason = format!("{error:#}");
        let expected = if fault == "missing_scope" {
            "store_unavailable: selected raw source identity mismatch"
        } else {
            "store_unavailable: selected raw source length or digest mismatch"
        };
        assert!(reason.contains(expected), "{fault}: {reason}");
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
            assert!(
                runtime.evidence_response().is_err(),
                "tool boundary did not latch recovery: {fault}"
            );
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

    let store = trellis::testing::retry_admission(|| Store::open_for_tests(temp.path(), &root));
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

// Incoming calls are checked through the actual MCP tools/call path rather than
// through the store reader or projection in isolation.
async fn incoming_many_documents_fixture(long_name: bool, limit: Option<usize>) -> (Value, Value) {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let name = if long_name {
        format!("Target{}", "Q".repeat(3_800))
    } else {
        "Target".to_owned()
    };
    fs::write(root.join("target.js"), format!("function {name}() {{}}\n")).unwrap();
    for number in 0..21 {
        fs::write(
            root.join(format!("caller{number:02}.js")),
            format!("function Caller{number:02}() {{ {name}(); }}\n"),
        )
        .unwrap();
    }
    let root_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .root_key;
    let (mut peer, witness) = Peer::start_with_causal_witness(Some(&root));
    tool(
        &peer.ask(call(
            false,
            json!(1),
            "trellis_workspace_describe",
            json!({"schemaVersion":1}),
        )),
        None,
        true,
    );
    let pin = await_settled_golden_h(&witness, &root_key).await;
    let found = peer.ask(call(
        false,
        json!(2),
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"Target","expectedBasis":pin}),
    ));
    tool(&found, None, true);
    let symbol_id = found["result"]["structuredContent"]["data"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == name)
        .unwrap()["symbolId"]
        .clone();
    let declaration = peer.ask(call(
        false,
        json!(3),
        "trellis_declaration",
        json!({"schemaVersion":1,"symbolId":symbol_id,"expectedBasis":pin}),
    ));
    tool(&declaration, None, true);
    let mut args = json!({"schemaVersion":1,"symbolId":symbol_id,"expectedBasis":pin});
    if let Some(limit) = limit {
        args["limit"] = json!(limit);
    }
    let incoming = peer.ask(call(false, json!(4), "trellis_incoming_calls", args));
    tool(&incoming, None, true);
    peer.assert_success("trellis mcp");
    (declaration, incoming)
}

fn assert_incoming_emitted_proof_closure(declaration: &Value, incoming: &Value) {
    use std::collections::BTreeSet;
    let selected = &declaration["result"]["structuredContent"]["data"];
    let selected_version = emitted_document(selected, selected);
    let data = &incoming["result"]["structuredContent"]["data"];
    let docs = data["documents"].as_array().unwrap();
    let calls = data["callers"].as_array().unwrap();
    assert_eq!(
        docs.len(),
        calls.len(),
        "each distinct emitted caller owns one document"
    );
    assert!(
        docs.iter()
            .all(|doc| doc["document"]["path"] != "target.js"),
        "selected declaration is proof closure, not a caller document ref"
    );
    let referenced: BTreeSet<_> = calls
        .iter()
        .map(|call| call["doc"].as_u64().unwrap() as usize)
        .collect();
    assert_eq!(referenced, (0..docs.len()).collect());
    let mut expected_proofs: BTreeSet<_> = docs
        .iter()
        .map(|doc| doc["provenanceId"].as_str().unwrap())
        .collect();
    expected_proofs.insert(selected_version["provenanceId"].as_str().unwrap());
    let actual_proofs: BTreeSet<_> = data["provenance"]
        .as_array()
        .unwrap()
        .iter()
        .map(|proof| proof["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        actual_proofs, expected_proofs,
        "selected + emitted callers only"
    );
    let mut expected_paths: BTreeSet<_> = docs
        .iter()
        .map(|doc| doc["document"]["path"].as_str().unwrap())
        .collect();
    expected_paths.insert("target.js");
    let actual_paths: BTreeSet<_> = data["coverage"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["documentPath"].as_str().unwrap())
        .collect();
    assert_eq!(
        actual_paths, expected_paths,
        "coverage includes selected and emitted sites only"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_default_six_limit_twenty_and_one_keep_exact_required_closure() {
    // #528: the default call page is six items.
    for (limit, expected) in [(None, 6), (Some(20), 20), (Some(1), 1)] {
        let (declaration, response) = incoming_many_documents_fixture(false, limit).await;
        let envelope = &response["result"]["structuredContent"];
        let data = &envelope["data"];
        assert_eq!(data["callers"].as_array().unwrap().len(), expected);
        assert_eq!(envelope["truncationReason"], "limit");
        assert_eq!(envelope["truncated"], true);
        assert_incoming_emitted_proof_closure(&declaration, &response);
        let docs = data["documents"].as_array().unwrap();
        assert_eq!(docs[0]["document"]["path"], "caller00.js");
        assert_eq!(
            docs.last().unwrap()["document"]["path"],
            format!("caller{:02}.js", expected - 1)
        );
        assert!(
            !docs
                .iter()
                .any(|doc| doc["document"]["path"] == "caller20.js")
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_byte_clipping_overrides_limit_and_discards_omitted_proofs() {
    let (declaration, response) = incoming_many_documents_fixture(true, Some(20)).await;
    let envelope = &response["result"]["structuredContent"];
    let data = &envelope["data"];
    let count = data["callers"].as_array().unwrap().len();
    assert!(
        count > 0 && count < 20,
        "required list must clip before 20: {count}"
    );
    assert_eq!(
        envelope["truncationReason"], "response_bytes",
        "byte clipping must take priority over the 21st-hit limit lookahead"
    );
    assert_eq!(envelope["truncated"], true);
    assert_incoming_emitted_proof_closure(&declaration, &response);
    let docs = data["documents"].as_array().unwrap();
    assert_eq!(
        docs.last().unwrap()["document"]["path"],
        format!("caller{:02}.js", count - 1)
    );
    assert!(
        !docs
            .iter()
            .any(|doc| doc["document"]["path"] == format!("caller{count:02}.js"))
    );
    assert!(serde_json::to_vec(&response).unwrap().len() < 65_536);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_escaped_javascript_declarations_keep_wire_labels_and_later_calls() {
    for (source, selected_name, expected_callee, later_name) in [
        (
            "function \\u{0000054}arget() {}\nfunction Normal() {}\nfunction Caller() { Target(); Normal(); }\n",
            "\\u{0000054}arget",
            Some("Target"),
            "Normal",
        ),
        (
            "class A { d\\u0065lete() {} normal() {} }\nfunction Caller() { normal(); const a = new A(); a.delete(); a.d\\u0065lete(); }\n",
            "d\\u0065lete",
            None,
            "normal",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        fs::write(root.join("a.js"), source).unwrap();
        let root_key = trellis::store::topology::WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .root_key;
        let (mut peer, witness) = Peer::start_with_causal_witness(Some(&root));
        tool(
            &peer.ask(call(
                false,
                json!(1),
                "trellis_workspace_describe",
                json!({"schemaVersion":1}),
            )),
            None,
            true,
        );
        let pin = await_settled_golden_h(&witness, &root_key).await;
        let find = |peer: &mut Peer, request_id: i64, query: &str| {
            let reply = peer.ask(call(
                false,
                json!(request_id),
                "trellis_find_symbols",
                json!({"schemaVersion":1,"query":query,"expectedBasis":pin}),
            ));
            tool(&reply, None, true);
            reply["result"]["structuredContent"]["data"]["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .find(|symbol| symbol["name"] == query)
                .unwrap_or_else(|| panic!("missing published {query}: {reply}"))["symbolId"]
                .clone()
        };
        let selected_id = find(&mut peer, 2, selected_name);
        let later_id = find(&mut peer, 3, later_name);
        let selected = peer.ask(call(
            false,
            json!(4),
            "trellis_declaration",
            json!({"schemaVersion":1,"symbolId":selected_id,"expectedBasis":pin}),
        ));
        tool(&selected, None, true);
        let decl = &selected["result"]["structuredContent"]["data"]["declaration"];
        assert_eq!(decl["name"], selected_name);
        assert_eq!(decl["syntaxId"], selected_id);
        let incoming = peer.ask(call(
            false,
            json!(5),
            "trellis_incoming_calls",
            json!({"schemaVersion":1,"symbolId":selected_id,"expectedBasis":pin}),
        ));
        tool(&incoming, None, true);
        let data = &incoming["result"]["structuredContent"]["data"];
        assert_eq!(data["symbolId"], selected_id);
        assert!(
            data["displayKey"].as_str().unwrap().contains(selected_name),
            "{incoming}"
        );
        assert_eq!(data["match"]["basis"], "name");
        let callers = data["callers"].as_array().unwrap();
        match expected_callee {
            Some(callee) => {
                assert_eq!(callers.len(), 1, "{incoming}");
                assert_eq!(callers[0]["calleeText"], callee);
                assert_eq!(callers[0]["matchedBy"], "name");
            }
            None => assert!(callers.is_empty(), "{incoming}"),
        }
        let caller_id = find(&mut peer, 6, "Caller");
        let later_incoming = peer.ask(call(
            false,
            json!(7),
            "trellis_incoming_calls",
            json!({"schemaVersion":1,"symbolId":later_id,"expectedBasis":pin}),
        ));
        tool(&later_incoming, None, true);
        let later_callers = later_incoming["result"]["structuredContent"]["data"]["callers"]
            .as_array()
            .unwrap();
        assert_eq!(later_callers.len(), 1, "{later_incoming}");
        assert_eq!(later_callers[0]["calleeText"], later_name);
        assert_eq!(later_callers[0]["ownerSyntaxId"], caller_id);
        peer.assert_success("trellis mcp");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn references_filtered_closure_fits_large_snapshot_and_selected_overflow_is_atomic() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let mut files: Vec<(String, String)> = (0..400)
        .map(|i| {
            (
                format!("module_{i:03}.js"),
                "export const value = 1;\n".into(),
            )
        })
        .collect();
    files[0].1 = "import './module_399.js'; import './module_399.js'; import 'external';\n".into();
    files[1].1 = "import('external');\n".repeat(300);
    files[2].1 = format!(
        "export const deep = {}{};\n",
        "[".repeat(400),
        "]".repeat(400)
    );
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let (identity, topology) = published_inactive_checkout(temp.path(), &borrowed);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(347, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 347, None);
    let mut ask = |id, arguments| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({
                    "name":"trellis_references", "arguments":arguments
                })
            )
        )
        .unwrap();
        next_reply(&mut reader)
    };
    let selected = ask(
        1,
        json!({"schemaVersion":1,"paths":["module_000.js"],"limit":1}),
    );
    tool(&selected, None, true);
    let envelope = &selected["result"]["structuredContent"];
    let basis = &envelope["evidenceBasis"];
    let data = &envelope["data"];
    let modules = data["modules"].as_array().unwrap();
    assert_eq!(modules.len(), 2);
    assert_eq!(modules[0]["identity"]["path"], "module_000.js");
    assert_eq!(modules[1]["identity"]["path"], "module_399.js");
    let edge = &data["production"][0];
    assert_eq!(edge["source"], modules[0]["identity"]);
    assert_eq!(edge["target"], modules[1]["identity"]);
    assert_eq!(edge["count"], 2);
    assert_eq!(edge["sites"].as_array().unwrap().len(), 1);
    assert_eq!(edge["omittedSites"], 1);
    // #785: exact kind counts, and the owner and target fields of every site.
    assert_eq!(
        edge["kindCounts"],
        json!({"call":0,"type":0,"import":2,"reference":0})
    );
    let site = &edge["sites"][0];
    assert_eq!(site["relationKind"], "import");
    assert_eq!(site["owner"]["syntaxId"], site["ownerSyntaxId"]);
    assert_eq!(site["owner"]["name"], Value::Null);
    assert_eq!(site["target"], Value::Null);
    assert_eq!(site["targetReason"], "structural_only");
    assert_eq!(data["unresolved"].as_array().unwrap().len(), 1);
    assert_eq!(data["unresolved"][0]["source"], modules[0]["identity"]);
    assert!(data["unresolved"][0]["site"]["owner"].is_object());
    assert_eq!(
        data["unresolved"][0]["site"]["targetReason"],
        "structural_only"
    );
    assert_eq!(
        data["counts"],
        json!({"scannedDocuments":1,"mappedSites":2,"unresolvedSites":1,"semanticSites":0})
    );
    assert_eq!(envelope["truncated"], false);
    // #554: the over-deep file is named, however the request is scoped.
    assert_eq!(data["partialFiles"].as_array().unwrap().len(), 1);
    assert_eq!(data["partialFiles"][0]["path"], "module_002.js");
    assert_eq!(data["partialFiles"][0]["reason"], "depth_limit");
    let empty = ask(
        2,
        json!({"schemaVersion":1,"expectedBasis":basis,"paths":["absent.js"],"limit":1}),
    );
    tool(&empty, None, true);
    let empty_data = &empty["result"]["structuredContent"]["data"];
    for field in ["modules", "production", "tests", "unresolved"] {
        assert_eq!(empty_data[field], json!([]));
    }
    assert_eq!(
        empty_data["counts"],
        json!({"scannedDocuments":0,"mappedSites":0,"unresolvedSites":0,"semanticSites":0})
    );
    assert_eq!(empty_data["semanticComplete"], false);
    let isolated = ask(
        3,
        json!({"schemaVersion":1,"expectedBasis":basis,"paths":["module_002.js"]}),
    );
    tool(&isolated, None, true);
    let isolated_data = &isolated["result"]["structuredContent"]["data"];
    assert_eq!(isolated_data["modules"].as_array().unwrap().len(), 1);
    assert_eq!(
        isolated_data["modules"][0]["identity"]["path"],
        "module_002.js"
    );
    assert_eq!(isolated_data["counts"]["scannedDocuments"], 1);
    // #448: one file's 300 unresolved sites exceed the response cap; the scoped
    // result now pages instead of failing response_too_large.
    let mut pages = Vec::new();
    let mut offset = 0;
    let mut unresolved = Vec::new();
    loop {
        let page = ask(
            4 + pages.len() as i32,
            json!({"schemaVersion":1,"expectedBasis":basis,"paths":["module_001.js"],"limit":1,"offset":offset}),
        );
        tool(&page, None, true);
        assert_reply_cap(&serde_json::to_vec(&page).unwrap());
        let data = &page["result"]["structuredContent"]["data"];
        assert_eq!(data["counts"]["unresolvedSites"], 300, "counts stay whole");
        assert_eq!(data["page"]["offset"], offset);
        unresolved.extend(data["unresolved"].as_array().unwrap().iter().cloned());
        let next = data["page"]["nextOffset"].as_u64();
        pages.push(page);
        match next {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert!(pages.len() > 1, "selected evidence spans several pages");
    assert_eq!(unresolved.len(), 300);
    for row in &unresolved {
        assert!(row["site"]["owner"].is_object());
        assert!(row["site"]["target"].is_null());
        assert_eq!(row["site"]["targetReason"], "structural_only");
    }
    let past = ask(
        90,
        json!({"schemaVersion":1,"expectedBasis":basis,"paths":["module_001.js"],"offset":10_000}),
    );
    tool(&past, Some("range_too_large"), true);
    let failed = &past["result"]["structuredContent"];
    assert_eq!(failed["error"]["currentBasis"], *basis);
    assert!(failed.get("data").is_none());
    let mut structured = vec![
        json!({"name":"trellis_references","structuredContent":envelope}),
        json!({"name":"trellis_references","structuredContent":empty["result"]["structuredContent"]}),
        json!({"name":"trellis_references","structuredContent":isolated["result"]["structuredContent"]}),
        json!({"name":"trellis_references","structuredContent":failed}),
    ];
    structured.extend(pages.iter().map(|page| {
        json!({"name":"trellis_references","structuredContent":page["result"]["structuredContent"]})
    }));
    validate_structured_content(&structured);
    drop(ask);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #775: `groupBy` directory and package rows sum the file answer's edges and unresolved
/// sites at the same pin, through MCP, and every answer fits the catalog schema.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn references_group_by_directory_and_package_sum_the_file_answer() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            (
                "src/app/main.js",
                "import '../lib/util.js'; import '../lib/util.js'; import './side.js'; import 'missing';\n",
            ),
            ("src/app/side.js", "export const s = 1;\n"),
            ("src/lib/util.js", "import 'absent'; export const u = 1;\n"),
        ],
    );
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(775, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 775, None);
    let mut ask = |id: i32, arguments: Value| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_references","arguments":arguments})
            )
        )
        .unwrap();
        next_reply(&mut reader)
    };
    let file = ask(1, json!({"schemaVersion":1}));
    tool(&file, None, true);
    let file = file["result"]["structuredContent"].clone();
    let basis = file["evidenceBasis"].clone();
    let data = &file["data"];
    assert_eq!(data["groupBy"], "file");
    assert_eq!(data["groups"], json!([]), "file mode emits no groups");
    let production: u64 = data["production"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["count"].as_u64().unwrap())
        .sum();
    assert_eq!(production, 3);
    assert_eq!(data["counts"]["unresolvedSites"], 2);
    let mut structured = vec![json!({"name":"trellis_references","structuredContent":file})];
    let mut grouped = |id: i32, arguments: Value| {
        let reply = ask(id, arguments);
        tool(&reply, None, true);
        let envelope = reply["result"]["structuredContent"].clone();
        assert_eq!(envelope["evidenceBasis"], basis, "same pin");
        structured.push(json!({"name":"trellis_references","structuredContent":envelope}));
        envelope["data"].clone()
    };
    let directory = grouped(
        2,
        json!({"schemaVersion":1,"expectedBasis":basis,"groupBy":"directory"}),
    );
    assert_eq!(directory["groupBy"], "directory");
    for field in ["modules", "production", "tests", "unresolved"] {
        assert_eq!(directory[field], json!([]), "{field}");
    }
    assert_eq!(
        directory["counts"], data["counts"],
        "counts are the file answer's"
    );
    assert_eq!(
        directory["groups"],
        json!([
            {"source":"src/app","target":"src/lib","sites":2,"production":2,"test":0},
            {"source":"src/app","target":"src/app","sites":1,"production":1,"test":0},
            {"source":"src/app","target":null,"sites":1,"production":1,"test":0},
            {"source":"src/lib","target":null,"sites":1,"production":1,"test":0},
        ])
    );
    let top = grouped(
        3,
        json!({"schemaVersion":1,"expectedBasis":basis,"groupBy":"directory","depth":1}),
    );
    assert_eq!(
        top["groups"],
        json!([
            {"source":"src","target":"src","sites":3,"production":3,"test":0},
            {"source":"src","target":null,"sites":2,"production":2,"test":0},
        ])
    );
    // #837: with no build manifest every file is in no app.
    let package = grouped(4, json!({"schemaVersion":1,"groupBy":"package"}));
    assert_eq!(
        package["groups"],
        json!([
            {"source":"noApp","target":"noApp","sites":3,"production":3,"test":0},
            {"source":"noApp","target":null,"sites":2,"production":2,"test":0},
        ])
    );
    // A row limit pages the same rows.
    let first = grouped(
        5,
        json!({"schemaVersion":1,"groupBy":"directory","limit":3}),
    );
    assert_eq!(first["groups"].as_array().unwrap().len(), 3);
    let next = first["page"]["nextOffset"].clone();
    assert!(next.is_u64(), "{first}");
    let rest = grouped(
        6,
        json!({"schemaVersion":1,"groupBy":"directory","limit":3,"offset":next}),
    );
    let mut paged = first["groups"].as_array().unwrap().clone();
    paged.extend(rest["groups"].as_array().unwrap().iter().cloned());
    assert_eq!(json!(paged), directory["groups"]);
    // Negative control: an unknown grouping is a typed invalid request, not a file answer.
    let bad = ask(7, json!({"schemaVersion":1,"groupBy":"module"}));
    tool(&bad, Some("invalid_request"), true);
    let bad_depth = ask(8, json!({"schemaVersion":1,"groupBy":"package","depth":1}));
    tool(&bad_depth, Some("invalid_request"), true);
    for reply in [&bad, &bad_depth] {
        structured.push(json!({"name":"trellis_references",
            "structuredContent":reply["result"]["structuredContent"]}));
    }
    drop(ask);
    validate_structured_content(&structured);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #837: `groupBy:"package"` groups by the #784 app (its manifest path), with `noApp` for a
/// file in no app, by the rules check's file assignment: a hidden layer's manifest is no
/// app, a single-app layer's file is that app's, and a shared layer's file falls back to the
/// app root that contains it. `prefix` keeps only the sources under a directory with every
/// `groupBy`, `depth` counts below it, and a bad prefix is a typed `invalid_request`. Every
/// answer fits the catalog schema.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn references_group_by_app_and_select_sources_under_a_prefix() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            ("web/package.json", "{\"name\":\"web\"}\n"),
            (
                "web/src/main.js",
                "import '../../lib/util.js'; import './view.js';\n",
            ),
            (
                "web/src/view.js",
                "import '../shared/s.js'; export const v = 1;\n",
            ),
            ("web/shared/s.js", "export const s = 1;\n"),
            ("admin/package.json", "{\"name\":\"admin\"}\n"),
            (
                "admin/main.js",
                "import '../lib/util.js'; import '../web/src/view.js';\n",
            ),
            ("lib/util.js", "export const u = 1;\n"),
            ("tools/fixture/package.json", "{\"name\":\"fixture\"}\n"),
            ("tools/fixture/run.js", "import '../../lib/util.js';\n"),
        ],
    );
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(837, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 837, None);
    let mut structured = Vec::new();
    let mut ask = |id: i32, arguments: Value, code: Option<&str>| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_references","arguments":arguments})
            )
        )
        .unwrap();
        let reply = next_reply(&mut reader);
        tool(&reply, code, true);
        let envelope = reply["result"]["structuredContent"].clone();
        structured.push(json!({"name":"trellis_references","structuredContent":envelope}));
        envelope
    };
    let row = |source: &str, target: &str, sites: u64| json!({"source":source,"target":target,"sites":sites,"production":sites,"test":0});
    // Without a rules file every manifest is an app, and a file belongs to the app whose
    // root most specifically contains it.
    let package = ask(1, json!({"schemaVersion":1,"groupBy":"package"}), None);
    let data = &package["data"];
    assert_eq!(data["groupBy"], "package");
    assert!(
        data.get("_pathApps").is_none(),
        "the file apps stay private"
    );
    assert_eq!(
        data["groups"],
        json!([
            row("web/package.json", "web/package.json", 2),
            row("admin/package.json", "noApp", 1),
            row("admin/package.json", "web/package.json", 1),
            row("tools/fixture/package.json", "noApp", 1),
            row("web/package.json", "noApp", 1),
        ])
    );
    let basis = package["evidenceBasis"].clone();
    let file = ask(2, json!({"schemaVersion":1,"expectedBasis":basis}), None);
    let mapped: u64 = file["data"]["production"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["count"].as_u64().unwrap())
        .sum();
    assert_eq!(mapped, 6, "the app rows sum the file answer");
    assert_eq!(package["data"]["counts"], file["data"]["counts"]);

    // A prefix selects sources with every grouping; `depth` counts below it.
    let web = ask(
        3,
        json!({"schemaVersion":1,"expectedBasis":basis,"prefix":"web/"}),
        None,
    );
    let sources: std::collections::BTreeSet<_> = web["data"]["production"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| edge["source"]["path"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        sources.into_iter().collect::<Vec<_>>(),
        ["web/src/main.js", "web/src/view.js"]
    );
    assert_eq!(web["data"]["counts"]["scannedDocuments"], 3);
    let both = ask(
        4,
        json!({"schemaVersion":1,"expectedBasis":basis,"prefix":"web","paths":["**/main.js"]}),
        None,
    );
    let edges = both["data"]["production"].as_array().unwrap();
    assert_eq!(edges.len(), 2);
    assert!(
        edges
            .iter()
            .all(|edge| edge["source"]["path"] == "web/src/main.js"),
        "paths and prefix both select"
    );
    let below = ask(
        5,
        json!({"schemaVersion":1,"expectedBasis":basis,"prefix":"web","groupBy":"directory","depth":1}),
        None,
    );
    assert_eq!(
        below["data"]["groups"],
        json!([
            row("web/src", "lib", 1),
            row("web/src", "web/shared", 1),
            row("web/src", "web/src", 1),
        ])
    );
    let admin = ask(
        6,
        json!({"schemaVersion":1,"expectedBasis":basis,"prefix":"admin","groupBy":"directory"}),
        None,
    );
    assert_eq!(
        admin["data"]["groups"],
        json!([row("admin", "lib", 1), row("admin", "web/src", 1)])
    );
    let admin_apps = ask(
        7,
        json!({"schemaVersion":1,"expectedBasis":basis,"prefix":"admin/","groupBy":"package"}),
        None,
    );
    assert_eq!(
        admin_apps["data"]["groups"],
        json!([
            row("admin/package.json", "noApp", 1),
            row("admin/package.json", "web/package.json", 1),
        ])
    );
    let nowhere = ask(
        8,
        json!({"schemaVersion":1,"expectedBasis":basis,"prefix":"nowhere","groupBy":"directory"}),
        None,
    );
    assert_eq!(nowhere["data"]["groups"], json!([]));
    assert_eq!(nowhere["data"]["counts"]["scannedDocuments"], 0);
    // A prefix is not a glob or a file: `web/src/main.js` has no source under it.
    let not_a_directory = ask(
        9,
        json!({"schemaVersion":1,"expectedBasis":basis,"prefix":"web/src/main.js"}),
        None,
    );
    assert_eq!(not_a_directory["data"]["production"], json!([]));

    // Invalid prefixes are typed invalid requests that name the field.
    for (id, prefix) in [
        (10, json!("../web")),
        (11, json!("/web")),
        (12, json!("web\\src")),
        (13, json!("web\u{0}")),
        (14, json!("")),
        (15, json!("web//src")),
        (16, json!("./web")),
        (17, json!(7)),
        (18, json!("a".repeat(4097))),
    ] {
        let failed = ask(
            id,
            json!({"schemaVersion":1,"groupBy":"directory","prefix":prefix}),
            Some("invalid_request"),
        );
        assert!(failed.get("data").is_none());
    }

    // The rules file's assignment: the hidden `tools` layer drops its manifest and gives its
    // file to `web`; the shared `lib` layer serves both apps, so `lib/util.js` falls back to
    // its root (none) and `web/shared/s.js` to `web`.
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n\
         [layers.web]\napp = \"web\"\norder = 1\npaths = [\"web/**\"]\n\
         [layers.admin]\napp = \"admin\"\norder = 1\npaths = [\"admin/**\"]\n\
         [layers.lib]\nshared = true\napps = [\"web\", \"admin\"]\n\
         paths = [\"lib/**\", \"web/shared/**\"]\n\
         [layers.fixture]\napp = \"web\"\norder = 2\ndisplay = \"hidden\"\n\
         paths = [\"tools/**\"]\n",
    )
    .unwrap();
    let ruled = ask(
        19,
        json!({"schemaVersion":1,"expectedBasis":basis,"groupBy":"package"}),
        None,
    );
    assert_eq!(
        ruled["data"]["groups"],
        json!([
            row("web/package.json", "noApp", 2),
            row("web/package.json", "web/package.json", 2),
            row("admin/package.json", "noApp", 1),
            row("admin/package.json", "web/package.json", 1),
        ])
    );
    // A rules file that check rejects fails app grouping as a typed `invalid_rules`, whose
    // location check reports; the other groupings never read it.
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.web]\norder = \"first\"\npaths = [\"web/**\"]\n",
    )
    .unwrap();
    let broken = ask(
        21,
        json!({"schemaVersion":1,"expectedBasis":basis,"groupBy":"package"}),
        Some("invalid_rules"),
    );
    assert!(broken["error"].get("diagnostic").is_none());
    ask(
        22,
        json!({"schemaVersion":1,"expectedBasis":basis,"groupBy":"directory","prefix":"admin"}),
        None,
    );
    drop(ask);
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n\
         [layers.web]\napp = \"web\"\norder = 1\npaths = [\"web/**\"]\n\
         [layers.admin]\napp = \"admin\"\norder = 1\npaths = [\"admin/**\"]\n\
         [layers.lib]\nshared = true\napps = [\"web\", \"admin\"]\n\
         paths = [\"lib/**\", \"web/shared/**\"]\n\
         [layers.fixture]\napp = \"web\"\norder = 2\ndisplay = \"hidden\"\n\
         paths = [\"tools/**\"]\n",
    )
    .unwrap();
    // The rules check counts the same apps: the hidden manifest is not one of them.
    writeln!(
        client,
        "{}",
        modern(
            json!(20),
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    let check = next_reply(&mut reader);
    tool(&check, None, true);
    let manifests: Vec<_> = check["result"]["structuredContent"]["data"]["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| app["manifest"].clone())
        .collect();
    assert_eq!(
        manifests,
        [json!("admin/package.json"), json!("web/package.json")]
    );
    validate_structured_content(&structured);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #908: on this checkout's shape (a Rust crate whose modules use each other and 52 external
/// crates), `groupBy:"package"` pages its in-checkout app rows from the first page, and the
/// external descriptors never fill a page ahead of them: each page lists only the descriptors
/// of its own rows' external targets, so a row `limit` bounds them too. Every page together
/// lists every descriptor of the file answer exactly once per page that targets it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn package_groups_lead_the_page_when_many_external_crates_are_used() {
    use trellis::daemon::registry::CheckoutRegistry;
    const CRATES: usize = 52;
    let temp = tempfile::tempdir().unwrap();
    let mut manifest =
        String::from("[package]\nname = \"shape\"\nversion = \"0.1.0\"\n\n[dependencies]\n");
    let mut lib = String::from("pub mod store;\npub mod mcp;\n");
    for n in 0..CRATES {
        manifest.push_str(&format!(
            "external_dependency_crate_{n:02} = {{ version = \"=1.2.3\", features = [\"full\"] }}\n"
        ));
        lib.push_str(&format!(
            "pub fn use_{n:02}() {{ external_dependency_crate_{n:02}::run(); }}\n"
        ));
    }
    manifest.push_str("\n[dev-dependencies]\n");
    for n in 0..CRATES {
        manifest.push_str(&format!("external_dependency_crate_{n:02} = \"1\"\n"));
    }
    let store = "pub fn open() {}\n";
    let mut mcp = String::new();
    for _ in 0..(CRATES + 8) {
        mcp.push_str("pub fn call() { crate::store::open(); }\n");
    }
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            ("Cargo.toml", manifest.as_str()),
            ("src/main.rs", "fn main() {}\n"),
            ("src/lib.rs", lib.as_str()),
            ("src/store.rs", store),
            ("src/mcp.rs", mcp.as_str()),
        ],
    );
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(908, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 908, None);
    let mut structured = Vec::new();
    let mut ask = |id: i32, arguments: Value| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_references","arguments":arguments})
            )
        )
        .unwrap();
        let reply = next_reply(&mut reader);
        tool(&reply, None, true);
        let envelope = reply["result"]["structuredContent"].clone();
        structured.push(json!({"name":"trellis_references","structuredContent":envelope}));
        envelope
    };
    let file = ask(1, json!({"schemaVersion":1}));
    let basis = file["evidenceBasis"].clone();
    let descriptors = |data: &Value| data["externalPackages"].as_array().unwrap().clone();
    // Page through the file answer for its whole descriptor list.
    let mut all = descriptors(&file["data"]);
    let mut next = file["data"]["page"]["nextOffset"].clone();
    let mut id = 2;
    while let Some(offset) = next.as_u64() {
        let page = ask(
            id,
            json!({"schemaVersion":1,"expectedBasis":basis,"offset":offset}),
        );
        all.extend(descriptors(&page["data"]));
        next = page["data"]["page"]["nextOffset"].clone();
        id += 1;
    }
    assert_eq!(all.len(), CRATES, "every crate is an emitted destination");

    let first = ask(
        id,
        json!({"schemaVersion":1,"expectedBasis":basis,"groupBy":"package"}),
    );
    let data = &first["data"];
    assert_eq!(data["groupBy"], "package");
    let groups = data["groups"].as_array().unwrap();
    assert_eq!(
        groups.first(),
        Some(&json!({"source":"Cargo.toml","target":"Cargo.toml",
            "sites":CRATES + 8,"production":CRATES + 8,"test":0})),
        "the in-checkout app row leads the first page"
    );
    // A page lists exactly the descriptors of its own rows' external targets.
    let targets = |data: &Value| -> Vec<Value> {
        let mut targets: Vec<Value> = data["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["target"].clone())
            .filter(Value::is_object)
            .collect();
        targets.sort_by_key(Value::to_string);
        targets.dedup();
        targets
    };
    let identities = |data: &Value| -> Vec<Value> {
        let mut identities: Vec<Value> = descriptors(data)
            .iter()
            .map(|package| package["identity"].clone())
            .collect();
        identities.sort_by_key(Value::to_string);
        identities
    };
    assert_eq!(identities(data), targets(data));
    assert_eq!(
        data["page"]["total"],
        CRATES + 1,
        "the sequence is the rows"
    );

    // Every page, with and without a row limit: rows concatenate to the whole sequence and
    // each crate's descriptor appears beside its row, no more than `limit` on a page.
    for limit in [None, Some(5)] {
        let mut offset = 0u64;
        let (mut rows, mut listed) = (Vec::new(), Vec::new());
        loop {
            id += 1;
            let mut arguments = json!({"schemaVersion":1,"expectedBasis":basis,"groupBy":"package","offset":offset});
            if let Some(limit) = limit {
                arguments["limit"] = json!(limit);
            }
            let page = ask(id, arguments);
            let data = &page["data"];
            assert_eq!(identities(data), targets(data));
            if let Some(limit) = limit {
                assert!(descriptors(data).len() <= limit);
                assert!(data["groups"].as_array().unwrap().len() <= limit);
            }
            rows.extend(data["groups"].as_array().unwrap().iter().cloned());
            listed.extend(descriptors(data));
            match data["page"]["nextOffset"].as_u64() {
                Some(next) => offset = next,
                None => break,
            }
        }
        assert_eq!(rows.len(), CRATES + 1, "{limit:?}");
        let key = |packages: &mut Vec<Value>| {
            packages.sort_by_key(Value::to_string);
        };
        key(&mut listed);
        key(&mut all);
        assert_eq!(listed, all, "{limit:?}: every descriptor, unchanged, once");
    }
    drop(ask);
    validate_structured_content(&structured);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #837 (Prime2 review of #861): `package` rejects the rules exactly when an unfiltered
/// `trellis_rules_check` does. An ambiguity only in a file that the read's `prefix` or
/// `paths` excludes, which nothing references, still fails `package` as `invalid_rules`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn package_groups_reject_rules_that_check_rejects_outside_the_selection() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            ("web/a.js", "export const a = 1;\n"),
            ("admin/b.js", "export const b = 2;\n"),
        ],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.one]\nshared = true\npaths = [\"admin/b.js\"]\n\
         [layers.two]\nshared = true\npaths = [\"admin/b.js\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(8371, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 8371, None);
    let mut structured = Vec::new();
    let mut ask = |id: i32, name: &str, arguments: Value, code: Option<&str>| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":name,"arguments":arguments})
            )
        )
        .unwrap();
        let reply = next_reply(&mut reader);
        tool(&reply, code, true);
        let envelope = reply["result"]["structuredContent"].clone();
        structured.push(json!({"name":name,"structuredContent":envelope}));
        envelope
    };
    ask(
        1,
        "trellis_rules_check",
        json!({"schemaVersion":1}),
        Some("invalid_rules"),
    );
    for (id, arguments) in [
        (
            2,
            json!({"schemaVersion":1,"groupBy":"package","prefix":"web"}),
        ),
        (
            3,
            json!({"schemaVersion":1,"groupBy":"package","paths":["web/**"]}),
        ),
        (
            4,
            json!({"schemaVersion":1,"groupBy":"package","prefix":"nowhere"}),
        ),
        (5, json!({"schemaVersion":1,"groupBy":"package"})),
    ] {
        let failed = ask(id, "trellis_references", arguments, Some("invalid_rules"));
        assert!(failed["error"].get("diagnostic").is_none());
    }
    // The groupings that never read the rules still answer.
    ask(
        6,
        "trellis_references",
        json!({"schemaVersion":1,"groupBy":"directory","prefix":"web"}),
        None,
    );
    drop(ask);
    validate_structured_content(&structured);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #837 with #794: `package` groups use the rules check's effective app list, so an authored
/// `[apps.<name>]` app (no manifest) is the group `[apps.<name>]` for the files under its
/// roots, beside detected manifest apps, and check counts the same inventory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn package_groups_use_authored_apps() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            ("web/package.json", "{\"name\":\"web\"}\n"),
            ("web/src/view.js", "export const v = 1;\n"),
            (
                "console/main.js",
                "import '../web/src/view.js'; import './panel.js';\n",
            ),
            ("console/panel.js", "export const p = 1;\n"),
            ("lib/util.js", "import '../console/panel.js';\n"),
        ],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[apps.console]\nroots = [\"console\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(8372, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 8372, None);
    let mut structured = Vec::new();
    let mut ask = |id: i32, name: &str, arguments: Value| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":name,"arguments":arguments})
            )
        )
        .unwrap();
        let reply = next_reply(&mut reader);
        tool(&reply, None, true);
        let envelope = reply["result"]["structuredContent"].clone();
        structured.push(json!({"name":name,"structuredContent":envelope}));
        envelope
    };
    let row = |source: &str, target: &str, sites: u64| json!({"source":source,"target":target,"sites":sites,"production":sites,"test":0});
    let package = ask(
        1,
        "trellis_references",
        json!({"schemaVersion":1,"groupBy":"package"}),
    );
    assert_eq!(
        package["data"]["groups"],
        json!([
            row("[apps.console]", "[apps.console]", 1),
            row("[apps.console]", "web/package.json", 1),
            row("noApp", "[apps.console]", 1),
        ])
    );
    let scoped = ask(
        2,
        "trellis_references",
        json!({"schemaVersion":1,"groupBy":"package","prefix":"lib"}),
    );
    assert_eq!(
        scoped["data"]["groups"],
        json!([row("noApp", "[apps.console]", 1)])
    );
    let check = ask(3, "trellis_rules_check", json!({"schemaVersion":1}));
    let inventory: Vec<_> = check["data"]["apps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|app| {
            (
                app["name"].clone(),
                app["source"].clone(),
                app["manifest"].clone(),
            )
        })
        .collect();
    assert_eq!(
        inventory,
        [
            (json!("console"), json!("override"), Value::Null),
            (json!("web"), json!("manifest"), json!("web/package.json")),
        ]
    );
    drop(ask);
    validate_structured_content(&structured);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #798: during an Architecture build a read with or without `paths` waits for the owner, then
/// answers the typed retryable `building` with the build's progress, inside its deadline. The
/// scoped read does not compute its own answer beside the build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_during_an_architecture_build_answers_building() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            ("a.js", "import './b.js';\n"),
            ("b.js", "export const b = 1;\n"),
        ],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.app]\norder = 1\npaths = [\"*.js\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(556, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    let scheduler = runtime.browser_scheduler().unwrap();
    // Startup's own build finishes first, so the hook below sees the build this test starts.
    runtime.await_architecture_for_tests();
    let (entered, reached) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    scheduler.set_architecture_hook_for_tests(std::sync::Arc::new(move |point| {
        // Past the build's read, which a publication at startup would have to wait for.
        if point == "resolving" {
            let _ = entered.send(());
            let _ = held
                .lock()
                .unwrap()
                .recv_timeout(trellis::testing::HANG_BACKSTOP);
        }
    }));
    // A rules edit needs a new core at this revision.
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.other]\norder = 1\npaths = [\"*.js\"]\n",
    )
    .unwrap();
    scheduler.architecture_hint();
    reached
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) =
        socket_session(&identity, &registry, 556, Some(Duration::from_secs(3)));
    let mut ask = |id: i64, arguments: Value| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_rules_check", "arguments":arguments})
            )
        )
        .unwrap();
        next_reply(&mut reader)
    };
    let builds = scheduler.architecture_builds_for_tests();
    let mut answers = Vec::new();
    for (id, arguments) in [
        (1, json!({"schemaVersion":1,"paths":["a.js"]})),
        (2, json!({"schemaVersion":1})),
    ] {
        let reply = ask(id, arguments);
        tool(&reply, Some("building"), true);
        let error = &reply["result"]["structuredContent"]["error"];
        assert_eq!(error["retryable"], true, "{reply}");
        assert_eq!(error["retryAfterMs"], 250, "{reply}");
        assert_eq!(
            error["progress"],
            json!({"phase":"resolving","completed":0,"total":2}),
            "{reply}"
        );
        assert_eq!(
            error["message"],
            "Architecture is being computed for this revision (resolving 0 of 2); retry shortly"
        );
        assert!(error["currentBasis"].is_object(), "{reply}");
        answers.push(json!({"name":"trellis_rules_check",
            "structuredContent":reply["result"]["structuredContent"]}));
    }
    validate_structured_content(&answers);
    // Neither read started another build; the scoped one answered without computing.
    assert_eq!(scheduler.architecture_builds_for_tests(), builds);
    drop(release);
    drop(ask);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// A read waiting on a build whose revision is passed waits again for the new head and answers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_waits_again_when_the_head_moves_during_its_wait() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            ("a.js", "import './b.js';\n"),
            ("b.js", "export const b = 1;\n"),
        ],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.app]\norder = 1\npaths = [\"*.js\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(557, &identity).unwrap();
    let runtime = registry.activate(&identity.root_key).unwrap();
    let scheduler = runtime.browser_scheduler().unwrap();
    runtime.await_architecture_for_tests();
    let (waiting, reader_waits) = std::sync::mpsc::channel();
    let (entered, reached) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let held = std::sync::Mutex::new(held);
    scheduler.set_architecture_hook_for_tests(std::sync::Arc::new(move |point| match point {
        "architecture_read_waiting" => {
            let _ = waiting.send(());
        }
        // Past the build's read, so the next publication does not wait for it.
        "resolving" => {
            let _ = entered.send(());
            let _ = held
                .lock()
                .unwrap()
                .recv_timeout(trellis::testing::HANG_BACKSTOP);
        }
        _ => {}
    }));
    // A rules edit starts a build at this revision that the read below waits for.
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.other]\norder = 1\npaths = [\"*.js\"]\n",
    )
    .unwrap();
    scheduler.architecture_hint();
    reached
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let (mut client, mut reader, session) =
        socket_session(&identity, &registry, 557, Some(Duration::from_secs(60)));
    writeln!(
        client,
        "{}",
        modern(
            json!(1),
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":{"schemaVersion":1}})
        )
    )
    .unwrap();
    reader_waits
        .recv_timeout(trellis::testing::HANG_BACKSTOP)
        .unwrap();
    let before = runtime
        .evidence_response()
        .unwrap()
        .0
        .status()
        .unwrap()
        .revision;
    fs::write(identity.root.join("c.js"), "export const c = 2;\n").unwrap();
    let indexing = runtime.clone();
    let root = identity.root.clone();
    tokio::task::spawn_blocking(move || {
        indexing.cli_index(&trellis::indexer::IndexOptions::new(root))
    })
    .await
    .unwrap()
    .unwrap();
    scheduler.architecture_hint();
    drop(release);
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let basis = &reply["result"]["structuredContent"]["evidenceBasis"];
    assert!(
        basis["indexRevision"].as_u64().unwrap() > before.index_revision,
        "{reply}"
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #448: a rules check scoped to one file with hundreds of unknown sites failed
/// `response_too_large`; it now samples evidence per instance and pages, while
/// counts and `referenceCount` stay exact and equal the CLI's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scoped_rules_check_samples_evidence_and_pages_under_the_cap() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let mut files: Vec<(String, String)> = (0..80)
        .map(|i| {
            (
                format!("lib_{i:03}.js"),
                format!("import './app_main.js';\nimport('external_{i}');\n"),
            )
        })
        .collect();
    files.push(("app_main.js".into(), "import('external');\n".repeat(300)));
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let (identity, topology) = published_inactive_checkout(temp.path(), &borrowed);
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.app]\norder = 1\npaths = [\"app_main.js\"]\n[layers.lib]\norder = 2\npaths = [\"lib_*.js\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(448, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 448, None);
    let mut ask = |id: i64, name: &str, arguments: Value| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":name, "arguments":arguments})
            )
        )
        .unwrap();
        next_reply(&mut reader)
    };
    let mut structured = Vec::new();
    let scoped = ask(
        1,
        "trellis_rules_check",
        json!({"schemaVersion":1,"paths":["app_main.js"]}),
    );
    tool(&scoped, None, true);
    assert_reply_cap(&serde_json::to_vec(&scoped).unwrap());
    let envelope = scoped["result"]["structuredContent"].clone();
    let basis = envelope["evidenceBasis"].clone();
    let data = &envelope["data"];
    let unknown = data["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["target"]["path"] == "app_main.js" && row["status"] == "unknown")
        .expect("aggregated unknown instance");
    assert_eq!(unknown["referenceCount"], 300);
    // #725: check keeps one witness per instance; explain samples by its `limit`.
    assert_eq!(unknown["evidence"].as_array().unwrap().len(), 1);
    assert_eq!(unknown["omittedEvidence"], 299);
    // #785: the sampled witness is classified and the kind counts cover every site.
    let counts = &unknown["kindCounts"];
    let total: u64 = ["call", "type", "import", "reference"]
        .iter()
        .map(|kind| counts[*kind].as_u64().unwrap())
        .sum();
    assert_eq!(total, 300, "{unknown}");
    let witness = &unknown["evidence"][0];
    assert_eq!(witness["relationKind"], "import", "{witness}");
    assert!(witness["lines"]["start"].as_u64().unwrap() >= 1);
    assert!(witness["spelling"].as_str().unwrap().contains("external"));
    assert!(witness["owner"].is_object(), "{witness}");
    assert_eq!(witness["targetReason"], "structural_only");
    assert!(witness["target"].is_null());
    assert_eq!(data["page"]["offset"], 0);
    // #784: with no detected app the check carries `apps: []` and `noApp` holds every count.
    assert_eq!(data["apps"], json!([]));
    assert_eq!(data["noApp"]["counts"], data["counts"]);
    structured.push(json!({"name":"trellis_rules_check","structuredContent":envelope.clone()}));
    // The unscoped check pages too; its concatenated instances keep every row.
    let mut offset = 0;
    let mut instances = 0;
    let mut counts: Option<Value> = None;
    let mut pages = 0;
    loop {
        let page = ask(
            10 + pages,
            "trellis_rules_check",
            json!({"schemaVersion":1,"expectedBasis":basis,"limit":50,"offset":offset}),
        );
        tool(&page, None, true);
        assert_reply_cap(&serde_json::to_vec(&page).unwrap());
        let data = &page["result"]["structuredContent"]["data"];
        let whole = counts.get_or_insert_with(|| data["counts"].clone());
        assert_eq!(
            data["counts"], *whole,
            "every page carries the whole-result counts"
        );
        assert_eq!(data["apps"], json!([]));
        assert_eq!(data["noApp"]["counts"], *whole);
        instances += data["instances"].as_array().unwrap().len();
        for row in data["instances"].as_array().unwrap() {
            if row["rule"] == "$layers" || row["rule"] == "$external" {
                let total: u64 = ["call", "type", "import", "reference"]
                    .iter()
                    .map(|kind| row["kindCounts"][*kind].as_u64().unwrap())
                    .sum();
                assert_eq!(total, row["referenceCount"].as_u64().unwrap(), "{row}");
                let witness = &row["evidence"][0];
                assert!(witness["relationKind"].is_string(), "{row}");
                assert!(witness["owner"].is_object(), "{row}");
                assert!(witness["targetReason"].is_string(), "{row}");
            }
        }
        structured.push(json!({"name":"trellis_rules_check",
            "structuredContent":page["result"]["structuredContent"]}));
        pages += 1;
        match data["page"]["nextOffset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert!(pages > 1);
    assert_eq!(
        instances as u64,
        counts.unwrap()["comparable"].as_u64().unwrap()
    );
    let explained = ask(
        2,
        "trellis_rules_explain",
        json!({"schemaVersion":1,"expectedBasis":basis,"rule":"$layers","target":unknown["target"],"limit":2}),
    );
    tool(&explained, None, true);
    let explained_data = &explained["result"]["structuredContent"]["data"];
    assert!(explained_data.get("apps").is_none(), "{explained_data}");
    assert!(explained_data.get("noApp").is_none(), "{explained_data}");
    let row = &explained_data["instances"][0];
    assert_eq!(row["evidence"].as_array().unwrap().len(), 2);
    assert_eq!(row["omittedEvidence"], 298);
    assert_eq!(row["kindCounts"]["import"], 300, "{row}");
    for item in row["evidence"].as_array().unwrap() {
        assert!(item["lines"]["start"].as_u64().unwrap() >= 1, "{item}");
        assert_eq!(item["relationKind"], "import", "{item}");
        assert!(item["spelling"].is_string(), "{item}");
        assert!(item["owner"].is_object(), "{item}");
        assert_eq!(item["targetReason"], "structural_only", "{item}");
    }
    structured.push(json!({"name":"trellis_rules_explain",
        "structuredContent":explained["result"]["structuredContent"]}));
    let references = ask(
        3,
        "trellis_references",
        json!({"schemaVersion":1,"expectedBasis":basis,"paths":["app_main.js"]}),
    );
    tool(&references, None, true);
    assert_eq!(
        references["result"]["structuredContent"]["data"]["counts"]["unresolvedSites"],
        300
    );
    structured.push(json!({"name":"trellis_references",
        "structuredContent":references["result"]["structuredContent"]}));
    validate_structured_content(&structured);
    drop(ask);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #725: through the daemon, `status` pages only the matching instances, `limit` counts
/// instances per page, and every page keeps the whole evaluation's counts.
#[tokio::test(flavor = "current_thread")]
async fn rules_check_status_filter_and_instance_limit_page_deviations() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let mut files: Vec<(String, String)> = (0..20)
        .map(|i| {
            (
                format!("lib_{i:03}.js"),
                format!("import './app_main.js';\nimport('external_{i}');\n"),
            )
        })
        .collect();
    files.push(("app_main.js".into(), "export const y = 1;\n".into()));
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let (identity, topology) = published_inactive_checkout(temp.path(), &borrowed);
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.app]\norder = 1\npaths = [\"app_main.js\"]\n[layers.lib]\norder = 2\npaths = [\"lib_*.js\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(725, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 725, None);
    let mut structured = Vec::new();
    let mut ask = |id: i64, arguments: Value| {
        let frame = modern(
            json!(id),
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":arguments}),
        );
        writeln!(client, "{frame}").unwrap();
        next_reply(&mut reader)
    };
    let whole = ask(1, json!({"schemaVersion":1}));
    tool(&whole, None, true);
    let counts = whole["result"]["structuredContent"]["data"]["counts"].clone();
    assert_eq!(counts["deviates"], 20, "{counts}");
    assert!(counts["unknown"].as_u64().unwrap() > 0, "{counts}");
    let mut offset = 0;
    let mut deviations = Vec::new();
    let mut sizes = Vec::new();
    for id in 10.. {
        let reply = ask(
            id,
            json!({"schemaVersion":1,"status":["deviates"],"limit":3,"offset":offset}),
        );
        tool(&reply, None, true);
        let envelope = reply["result"]["structuredContent"].clone();
        let data = &envelope["data"];
        assert_eq!(data["counts"], counts, "counts stay global");
        assert_eq!(data["page"]["status"], json!(["deviates"]));
        assert!(data["semanticBases"].is_array(), "{data}");
        let rows = data["instances"].as_array().unwrap();
        assert!(rows.iter().all(|row| row["status"] == "deviates"), "{data}");
        sizes.push(rows.len());
        deviations.extend(rows.iter().map(|row| row["target"]["path"].clone()));
        structured.push(json!({"name":"trellis_rules_check","structuredContent":envelope.clone()}));
        match data["page"]["nextOffset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(deviations.len(), 20);
    assert!(sizes[..6].iter().all(|size| *size == 3), "{sizes:?}");
    let invalid = ask(2, json!({"schemaVersion":1,"status":["violates"]}));
    tool(&invalid, Some("invalid_request"), true);
    drop(ask);
    validate_structured_content(&structured);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn rules_check_paths_scope_the_unassigned_list_through_every_page() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let mut files: Vec<(String, String)> = (0..60)
        .map(|i| {
            (
                format!("loose_{i:03}.js"),
                "export const x = 1;\n".to_owned(),
            )
        })
        .collect();
    files.push(("app_main.js".into(), "export const y = 1;\n".into()));
    let borrowed: Vec<_> = files
        .iter()
        .map(|(p, t)| (p.as_str(), t.as_str()))
        .collect();
    let (identity, topology) = published_inactive_checkout(temp.path(), &borrowed);
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.app]\norder = 1\npaths = [\"app_main.js\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(559, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 559, None);
    let mut structured = Vec::new();
    let mut ask = |id: i64, arguments: Value| {
        let frame = modern(
            json!(id),
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":arguments}),
        );
        writeln!(client, "{frame}").unwrap();
        let reply = next_reply(&mut reader);
        tool(&reply, None, true);
        let envelope = reply["result"]["structuredContent"].clone();
        structured.push(json!({"name":"trellis_rules_check","structuredContent":envelope.clone()}));
        envelope["data"].clone()
    };
    let paths = |data: &Value| -> Vec<String> {
        data["unassigned"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["path"].as_str().unwrap().to_owned())
            .collect()
    };
    // The file the issue checked: assigned, so nothing unrelated comes back.
    let scoped = ask(1, json!({"schemaVersion":1,"paths":["app_main.js"]}));
    assert_eq!(paths(&scoped), Vec::<String>::new());
    assert_eq!(scoped["page"]["total"], 0);
    assert_eq!(
        scoped["unassignedFileCount"], 60,
        "the total stays checkout-wide"
    );
    let one = ask(2, json!({"schemaVersion":1,"paths":["loose_007.js"]}));
    assert_eq!(paths(&one), ["loose_007.js"]);
    assert_eq!(one["page"]["total"], 1);
    let some = ask(3, json!({"schemaVersion":1,"paths":["loose_00*.js"]}));
    assert_eq!(some["page"]["total"], 10);
    // Without paths, the whole catalog is still there.
    let mut all = Vec::new();
    let mut offset = 0;
    let mut id = 10;
    loop {
        let data = ask(id, json!({"schemaVersion":1,"offset":offset}));
        assert_eq!(data["unassignedFileCount"], 60);
        all.extend(paths(&data));
        id += 1;
        match data["page"]["nextOffset"].as_u64() {
            Some(next) => offset = next,
            None => break,
        }
    }
    assert_eq!(all.len(), 60);
    drop(ask);
    validate_structured_content(&structured);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #769: a check instance that omits evidence carries a ready-made `nextCall`. Its
/// `trellis_rules_explain` arguments, sent back verbatim, succeed and return that
/// instance with all of its evidence. An instance that omits nothing has no hint.
#[tokio::test(flavor = "current_thread")]
async fn rules_check_next_call_explains_an_instance_with_its_omitted_evidence() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            (
                "app_main.js",
                "import('external');\nimport('external');\nimport('external');\n",
            ),
            (
                "lib_000.js",
                "import './app_main.js';\nimport { y } from './app_main.js';\n",
            ),
            ("lib_001.js", "import './app_main.js';\n"),
        ],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        "version = 1\n[layers.app]\norder = 1\npaths = [\"app_main.js\"]\n[layers.lib]\norder = 2\npaths = [\"lib_*.js\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(769, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 769, None);
    let mut ask = |id: i64, name: &str, arguments: Value| {
        let frame = modern(
            json!(id),
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        );
        writeln!(client, "{frame}").unwrap();
        next_reply(&mut reader)
    };
    let check = ask(1, "trellis_rules_check", json!({"schemaVersion":1}));
    tool(&check, None, true);
    let mut structured = vec![
        json!({"name":"trellis_rules_check","structuredContent":check["result"]["structuredContent"]}),
    ];
    let data = &check["result"]["structuredContent"]["data"];
    let rows: Vec<Value> = ["instances", "testOnly"]
        .iter()
        .flat_map(|field| data[*field].as_array().unwrap().clone())
        .collect();
    let mut hinted = 0;
    for (index, row) in rows.iter().enumerate() {
        if row["omittedEvidence"] == 0 {
            assert!(row.get("nextCall").is_none(), "{row}");
            continue;
        }
        hinted += 1;
        let next = &row["nextCall"];
        assert_eq!(next["tool"], "trellis_rules_explain", "{row}");
        let arguments = &next["arguments"];
        assert_eq!(arguments["rule"], row["rule"]);
        assert_eq!(arguments["target"], row["target"]);
        assert_eq!(
            arguments.get("destination").unwrap_or(&Value::Null),
            &row["destination"]
        );
        let explained = ask(
            10 + index as i64,
            "trellis_rules_explain",
            arguments.clone(),
        );
        tool(&explained, None, true);
        let instances = explained["result"]["structuredContent"]["data"]["instances"]
            .as_array()
            .unwrap();
        let same = instances
            .iter()
            .find(|candidate| {
                candidate["rule"] == row["rule"]
                    && candidate["target"] == row["target"]
                    && candidate["destination"] == row["destination"]
                    && candidate["status"] == row["status"]
            })
            .unwrap_or_else(|| panic!("explain lists the hinted instance: {explained}"));
        let total = row["omittedEvidence"].as_u64().unwrap() + 1;
        assert_eq!(
            same["evidence"].as_array().unwrap().len() as u64,
            total,
            "{same}"
        );
        assert_eq!(same["omittedEvidence"], 0, "{same}");
        assert_eq!(same["evidence"][0], row["evidence"][0]);
        structured.push(json!({"name":"trellis_rules_explain",
            "structuredContent":explained["result"]["structuredContent"]}));
    }
    // The unknown app_main.js instance (three unresolved imports) and the lib_000.js
    // deviation (two imports of app_main.js) omit evidence; lib_001.js does not.
    assert_eq!(hinted, 2, "{data}");
    validate_structured_content(&structured);
    drop(ask);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #769: without `.trellis/rules.toml`, check and explain fail `rules_not_found` with a
/// static hint naming the `trellis_propose_rules` prompt that drafts one.
#[tokio::test(flavor = "current_thread")]
async fn rules_not_found_names_the_propose_rules_prompt() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) =
        published_inactive_checkout(temp.path(), &[("app.js", "export const a = 1;\n")]);
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(7691, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 7691, None);
    let target = json!({"sourceSetId":"app","language":"javascript","path":"app.js",
        "sid":"sid:v1:00000000000000000000000000000000"});
    for (id, name, arguments) in [
        (1, "trellis_rules_check", json!({"schemaVersion":1})),
        (
            2,
            "trellis_rules_explain",
            json!({"schemaVersion":1,"rule":"$layers","target":target}),
        ),
    ] {
        let frame = modern(
            json!(id),
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        );
        writeln!(client, "{frame}").unwrap();
        let reply = next_reply(&mut reader);
        tool(&reply, Some("rules_not_found"), true);
        let envelope = &reply["result"]["structuredContent"];
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(message.contains(".trellis/rules.toml"), "{message}");
        assert!(message.contains("`trellis_propose_rules`"), "{message}");
        assert!(
            message.contains("/mcp__trellis__trellis_propose_rules"),
            "{message}"
        );
        assert_eq!(
            reply["result"]["content"][0]["text"],
            format!("{name} failed: rules_not_found: {message}")
        );
        validate_structured_content(&[json!({"name":name,"structuredContent":envelope})]);
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #769: an `invalid_request` for tool arguments names the failing field and the
/// shape it expects, in static server text. A `{language,path}` module pair (the
/// rules.toml exception shape) or a stringified identity is not an explain `target`.
#[test]
fn invalid_tool_arguments_name_the_failing_field_and_its_shape() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let mut peer = Peer::start(Some(&root));
    let identity = json!({"sourceSetId":"app","language":"rust","path":"src/a.rs",
        "sid":"sid:v1:00000000000000000000000000000000"});
    let target_message = "`target` must be the identity object from a trellis_rules_check \
        instance: {sourceSetId, language, path, sid}";
    let cases = [
        (
            "trellis_rules_explain",
            json!({"schemaVersion":1,"rule":"$layers",
                "target":{"language":"rust","path":"src/a.rs"}}),
            target_message,
        ),
        // The Claude Desktop shape: the right object, sent as a JSON string. The closed
        // schema still rejects it, and the message says what arrived.
        (
            "trellis_rules_explain",
            json!({"schemaVersion":1,"rule":"$layers","target":identity.to_string()}),
            "`target` must be a JSON object (got a string): the identity object from a \
                trellis_rules_check instance, {sourceSetId, language, path, sid}",
        ),
        (
            "trellis_rules_explain",
            json!({"schemaVersion":1,"rule":"$layers","target":identity.clone(),
                "destination":identity.to_string()}),
            "`destination` must be a JSON object (got a string): the destination object \
                from a trellis_rules_check instance",
        ),
        (
            "trellis_rules_check",
            json!({"schemaVersion":1,"expectedBasis":
                json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000",
                    "indexRevision":1}).to_string()}),
            "`expectedBasis` must be a JSON object (got a string): the evidenceBasis \
                object of an earlier result, {indexGeneration, indexRevision}",
        ),
        (
            "trellis_rules_explain",
            json!({"schemaVersion":1,"rule":"$layers"}),
            target_message,
        ),
        (
            "trellis_rules_explain",
            json!({"schemaVersion":1,"rule":"$layers","target":identity,
                "destination":{"language":"rust","path":"src/b.rs"}}),
            "`destination` must be the destination object from a trellis_rules_check \
                instance: {sourceSetId, language, path, sid} or {kind:\"externalPackage\", \
                sourceSetId, language, package, standardLibrary}, and only for $layers or \
                $external",
        ),
        (
            "trellis_rules_check",
            json!({"schemaVersion":1,"status":["violates"]}),
            "`status` must be 1 to 3 distinct values from \"conforms\", \"deviates\", \"unknown\"",
        ),
        (
            "trellis_declaration",
            json!({"schemaVersion":1,"symbolId":"sid:v1:x"}),
            "`symbolId` must be a syntax ID: sid:v1: followed by 32 lowercase hex digits",
        ),
    ];
    for (id, (name, arguments, message)) in cases.into_iter().enumerate() {
        let reply = peer.ask(modern(
            json!(id),
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        ));
        tool(&reply, Some("invalid_request"), true);
        assert_eq!(
            reply["result"]["structuredContent"]["error"]["message"], message,
            "{name} {arguments}"
        );
        assert_eq!(
            reply["result"]["content"][0]["text"],
            format!("{name} failed: invalid_request: {message}")
        );
    }
    peer.assert_success("trellis mcp");
}

/// #769: every input property whose `$ref` names an object-valued definition also says
/// `"type":"object"`, so a client serializes it as an object rather than a string.
#[test]
fn object_valued_input_refs_declare_type_object() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let mut peer = Peer::start(Some(&root));
    let list = peer.ask(modern(json!(1), "tools/list", json!({})));
    let mut checked = Vec::new();
    for tool in list["result"]["tools"].as_array().unwrap() {
        let schema = &tool["inputSchema"];
        for (field, property) in schema["properties"].as_object().unwrap() {
            let Some(reference) = property.get("$ref").and_then(Value::as_str) else {
                continue;
            };
            let definition = &schema["$defs"][reference.strip_prefix("#/$defs/").unwrap()];
            let object = definition["type"] == "object"
                || definition["oneOf"].as_array().is_some_and(|branches| {
                    branches.iter().all(|branch| {
                        let name = branch["$ref"].as_str().unwrap_or_default();
                        schema["$defs"][name.strip_prefix("#/$defs/").unwrap_or_default()]["type"]
                            == "object"
                    })
                });
            if object {
                assert_eq!(property["type"], "object", "{} {field}", tool["name"]);
                checked.push(format!("{}.{field}", tool["name"].as_str().unwrap()));
            } else {
                assert!(property.get("type").is_none(), "{} {field}", tool["name"]);
            }
        }
    }
    assert!(checked.contains(&"trellis_rules_explain.target".to_owned()));
    assert!(checked.contains(&"trellis_rules_explain.destination".to_owned()));
    assert!(checked.contains(&"trellis_find_symbols.expectedBasis".to_owned()));
    assert_eq!(checked.len(), 14, "{checked:?}");
    // Nested: the explain destination union and each of its branches say so too.
    let explain = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "trellis_rules_explain")
        .unwrap();
    let destination = &explain["inputSchema"]["$defs"]["Destination"];
    assert_eq!(destination["type"], "object");
    for branch in destination["oneOf"].as_array().unwrap() {
        assert_eq!(branch["type"], "object", "{branch}");
    }
    peer.assert_success("trellis mcp");
}

#[test]
fn architecture_catalog_prompt_and_closed_inputs_use_both_protocol_modes() {
    for legacy in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        let mut peer = Peer::start(Some(&root));
        if legacy {
            ready_legacy(&mut peer);
        }
        let frame = |id: i64, method: &str, params: Value| {
            if legacy {
                request(json!(id), method, params)
            } else {
                modern(json!(id), method, params)
            }
        };
        let list = peer.ask(frame(101, "prompts/list", json!({})));
        assert_eq!(
            list["result"]["prompts"][0]["name"],
            "trellis_propose_rules"
        );
        assert_eq!(list["result"]["prompts"][0]["arguments"], json!([]));
        assert_eq!(list["result"].get("resultType").is_some(), !legacy);
        for (id, method, params) in [
            (102, "prompts/list", json!({"cursor":"next"})),
            (103, "prompts/get", json!({"name":"unknown"})),
            // #794: the old prompt name has no alias.
            (
                110,
                "prompts/get",
                json!({"name":"trellis_propose_layers","arguments":{}}),
            ),
            (
                104,
                "prompts/get",
                json!({"name":"trellis_propose_rules","arguments":{"scope":"src"}}),
            ),
            (
                105,
                "prompts/get",
                json!({"name":"trellis_propose_rules","arguments":null}),
            ),
            (
                106,
                "tools/call",
                json!({"name":"trellis_propose_rules","arguments":{"schemaVersion":1}}),
            ),
        ] {
            error(&peer.ask(frame(id, method, params)), -32602, json!(id));
        }
        let cold = peer.ask(frame(
            107,
            "prompts/get",
            json!({"name":"trellis_propose_rules","arguments":{}}),
        ));
        assert_eq!(cold["error"]["code"], -32603, "{cold}");
        assert_eq!(cold["error"]["data"]["code"], "index_not_ready", "{cold}");
        assert!(cold.get("result").is_none());
        for (id, name) in [(108, "trellis_references"), (109, "trellis_rules_check")] {
            let reply = peer.ask(frame(
                id,
                "tools/call",
                json!({"name":name,"arguments":{"schemaVersion":1}}),
            ));
            tool(&reply, Some("index_not_ready"), !legacy);
        }
        peer.assert_success("trellis mcp");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn architecture_tools_and_prompt_share_pinned_evidence_and_current_rules() {
    use trellis::daemon::registry::CheckoutRegistry;
    for legacy in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir_all(temp.path().join("checkout/src")).unwrap();
        let (identity, topology) = published_inactive_checkout(
            temp.path(),
            &[
                ("src/a.rs", "pub fn a() { crate::b::b(); }\n"),
                ("src/b.rs", "pub fn b() {}\n"),
                ("src/lib.rs", "mod a; mod b;\n"),
                (
                    "Cargo.toml",
                    "[package]\nname='boundary'\nversion='0.1.0'\n",
                ),
            ],
        );
        fs::create_dir(identity.root.join(".trellis")).unwrap();
        let rules = "version = 1\n[layers.top]\norder = 2\npaths = [\"src/a.rs\"]\n[layers.bottom]\norder = 1\npaths = [\"src/b.rs\"]\n";
        fs::write(identity.root.join(".trellis/rules.toml"), rules).unwrap();
        let mut registry = CheckoutRegistry::with_roots(topology);
        registry.attach_launch(129, &identity).unwrap();
        let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
        architecture_built(&registry, &identity).await;
        let (mut client, mut reader, session) = socket_session(&identity, &registry, 129, None);
        let frame = |id: i64, method: &str, params: Value| {
            if legacy {
                request(json!(id), method, params)
            } else {
                modern(json!(id), method, params)
            }
        };
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
            assert!(next_reply(&mut reader)["result"].is_object());
            writeln!(
                client,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/initialized"})
            )
            .unwrap();
        }
        let mut ask = |id, method, params| {
            writeln!(client, "{}", frame(id, method, params)).unwrap();
            next_reply(&mut reader)
        };
        let refs = ask(
            1,
            "tools/call",
            json!({"name":"trellis_references","arguments":{"schemaVersion":1}}),
        );
        tool(&refs, None, !legacy);
        let refs_env = &refs["result"]["structuredContent"];
        assert_eq!(
            refs_env["data"]["production"].as_array().unwrap().len(),
            1,
            "{refs}"
        );
        let basis = refs_env["evidenceBasis"].clone();
        let checked = ask(
            2,
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":{"schemaVersion":1,"expectedBasis":basis}}),
        );
        tool(&checked, None, !legacy);
        let checked_env = &checked["result"]["structuredContent"];
        assert_eq!(checked_env["evidenceBasis"], basis);
        assert_eq!(checked_env["data"]["counts"]["deviates"], 1, "{checked}");
        let instance = &checked_env["data"]["instances"][0];
        let call_site = instance["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|site| !site["callId"].is_null())
            .expect("native call site");
        assert_eq!(call_site["caller"]["stableLookupKey"], "src/a.rs::a");
        assert!(call_site["callerReason"].is_null());
        let explain = ask(
            3,
            "tools/call",
            json!({"name":"trellis_rules_explain","arguments":{
            "schemaVersion":1,"expectedBasis":basis,"rule":instance["rule"],"target":instance["target"],"destination":instance["destination"]}}),
        );
        tool(&explain, None, !legacy);
        assert_eq!(
            explain["result"]["structuredContent"]["data"]["instances"],
            checked_env["data"]["instances"]
        );
        assert_eq!(
            explain["result"]["structuredContent"]["data"]["instances"][0]["evidence"],
            instance["evidence"]
        );
        let outgoing = ask(
            20,
            "tools/call",
            json!({"name":"trellis_outgoing_calls","arguments":{
                "schemaVersion":1,"lookupKey":call_site["caller"]["stableLookupKey"],"expectedBasis":basis
            }}),
        );
        tool(&outgoing, None, !legacy);
        assert_eq!(
            outgoing["result"]["structuredContent"]["data"]["calls"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        fs::write(identity.root.join(".trellis/rules.toml"), format!("{rules}\n[[exceptions]]\nrule='$layers'\ntarget={{language='rust',path='src/a.rs'}}\ndestination={{language='rust',path='src/b.rs'}}\nreason='Reviewed boundary'\n")).unwrap();
        let excepted = ask(
            21,
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":{
                "schemaVersion":1,"expectedBasis":basis
            }}),
        );
        tool(&excepted, None, !legacy);
        let excepted_row = &excepted["result"]["structuredContent"]["data"]["instances"][0];
        assert_eq!(excepted_row["excepted"], true);
        assert_eq!(excepted_row["evidence"], instance["evidence"]);
        validate_structured_content(&[
            json!({"name":"trellis_rules_check","structuredContent":excepted["result"]["structuredContent"]}),
            json!({"name":"trellis_outgoing_calls","structuredContent":outgoing["result"]["structuredContent"]}),
        ]);
        fs::write(identity.root.join(".trellis/rules.toml"), rules).unwrap();
        let mut absent_destination = instance["destination"].clone();
        absent_destination["sid"] = json!(format!("sid:v1:{}", "f".repeat(32)));
        let not_found = ask(
            10,
            "tools/call",
            json!({"name":"trellis_rules_explain","arguments":{
                "schemaVersion":1,"expectedBasis":basis,"rule":"$layers",
                "target":instance["target"],"destination":absent_destination}}),
        );
        tool(&not_found, Some("not_found"), !legacy);
        let not_found_env = &not_found["result"]["structuredContent"];
        assert_eq!(not_found_env["workspace"], refs_env["workspace"]);
        assert!(not_found_env["catchingUp"].is_boolean());
        let current_basis = &not_found_env["error"]["currentBasis"];
        assert_eq!(current_basis["indexGeneration"], basis["indexGeneration"]);
        assert!(
            current_basis["indexRevision"].as_u64().unwrap()
                >= basis["indexRevision"].as_u64().unwrap()
        );
        assert_eq!(not_found_env["error"]["retryable"], false);
        assert!(not_found_env.get("data").is_none());
        validate_structured_content(&[
            json!({"name":"trellis_rules_explain","structuredContent":not_found_env}),
        ]);
        let prompt = ask(4, "prompts/get", json!({"name":"trellis_propose_rules"}));
        assert!(prompt.get("error").is_none(), "{prompt}");
        let text = prompt["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(text.contains("for human review"));
        assert!(
            text.contains(
                checked_env["data"]["rulesFile"]["contentHash"]
                    .as_str()
                    .unwrap()
            )
        );
        assert!(text.contains(basis["indexGeneration"].as_str().unwrap()));
        validate_structured_content(&[
            json!({"name":"trellis_references","structuredContent":refs_env}),
            json!({"name":"trellis_rules_check","structuredContent":checked_env}),
            json!({"name":"trellis_rules_explain","structuredContent":explain["result"]["structuredContent"]}),
        ]);
        fs::write(
            identity.root.join(".trellis/rules.toml"),
            "version = 1\nunknown = true\n",
        )
        .unwrap();
        let invalid = ask(
            5,
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":{"schemaVersion":1}}),
        );
        tool(&invalid, Some("invalid_rules"), !legacy);
        assert!(invalid["result"]["structuredContent"]["error"]["diagnostic"].is_object());
        validate_structured_content(&[
            json!({"name":"trellis_rules_check","structuredContent":invalid["result"]["structuredContent"]}),
        ]);
        let bad_prompt = ask(6, "prompts/get", json!({"name":"trellis_propose_rules"}));
        assert_eq!(
            bad_prompt["error"]["data"]["code"], "invalid_rules",
            "{bad_prompt}"
        );
        assert!(bad_prompt.get("result").is_none());
        fs::remove_file(identity.root.join(".trellis/rules.toml")).unwrap();
        let absent = ask(7, "prompts/get", json!({"name":"trellis_propose_rules"}));
        assert!(
            absent["result"]["messages"][0]["content"]["text"]
                .as_str()
                .unwrap()
                .contains("\"rulesText\":null")
        );
        let absent_text = absent["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap();
        assert!(absent_text.contains("\"apps\":[]"), "{absent_text}");
        assert!(!absent_text.contains("PER-APP LAYERS"));
        let missing = ask(
            8,
            "tools/call",
            json!({"name":"trellis_rules_check","arguments":{"schemaVersion":1}}),
        );
        tool(&missing, Some("rules_not_found"), !legacy);
        let old_pin =
            json!({"indexGeneration":"123e4567-e89b-42d3-a456-426614174000","indexRevision":0});
        let stale = ask(
            9,
            "tools/call",
            json!({"name":"trellis_references","arguments":{"schemaVersion":1,"expectedBasis":old_pin}}),
        );
        tool(&stale, Some("revision_conflict"), !legacy);
        drop(ask);
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
}

/// #770: on a checkout whose structural references alone exceed the 65,536-byte frame,
/// `prompts/get` still returns a fitting, deterministic proposal context: directory
/// edges and file-module pairs with exact totals and omitted counts, and the exact
/// current rules bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn propose_rules_prompt_fits_the_frame_when_references_exceed_it() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let mut files = Vec::new();
    for package in 0..10 {
        fs::create_dir_all(temp.path().join(format!("checkout/pkg{package}"))).unwrap();
        files.push((format!("pkg{package}/__init__.py"), String::new()));
        for module in 0..20 {
            let mut text = String::new();
            for other in 1..=6 {
                let target = (package + other) % 10;
                let name = (module + other) % 20;
                text.push_str(&format!("from pkg{target}.m{name} import f as f{other}\n"));
            }
            text.push_str("def f():\n    pass\n");
            files.push((format!("pkg{package}/m{module}.py"), text));
        }
    }
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let (identity, topology) = published_inactive_checkout(temp.path(), &borrowed);
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    let rules = "version = 1\n[layers.top]\norder = 1\npaths = [\"pkg0/**\"]\n";
    fs::write(identity.root.join(".trellis/rules.toml"), rules).unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(770, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 770, None);
    let mut raw = |id: i64, method: &str, params: Value| {
        writeln!(client, "{}", modern(json!(id), method, params)).unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        line
    };
    let refs: Value = serde_json::from_str(&raw(
        1,
        "tools/call",
        json!({"name":"trellis_references","arguments":{"schemaVersion":1}}),
    ))
    .unwrap();
    let refs_env = &refs["result"]["structuredContent"];
    assert!(refs_env.get("error").is_none(), "{refs}");
    let first = raw(2, "prompts/get", json!({"name":"trellis_propose_rules"}));
    // A small prompt: instructions, rules, pin, totals and a bounded overview.
    assert!(first.len() < 32 * 1024, "{} bytes", first.len());
    let prompt: Value = serde_json::from_str(&first).unwrap();
    assert!(prompt.get("error").is_none(), "{prompt}");
    let second = raw(3, "prompts/get", json!({"name":"trellis_propose_rules"}));
    assert_eq!(
        first.replacen("\"id\":2", "", 1),
        second.replacen("\"id\":3", "", 1),
        "deterministic"
    );
    let text = prompt["result"]["messages"][0]["content"]["text"]
        .as_str()
        .unwrap();
    assert!(text.contains("for human review"));
    assert!(text.contains("WORKFLOW\n1. Start from the overview"));
    assert!(text.contains("trellis_references with expectedBasis"));
    let context: Value =
        serde_json::from_str(text.split("UNTRUSTED REPOSITORY DATA\n").nth(1).unwrap()).unwrap();
    let payload = &context["payload"];
    assert_eq!(payload["rulesText"], rules);
    assert_eq!(
        context["evidenceBasis"], refs_env["evidenceBasis"],
        "one pin"
    );
    let summary = &payload["references"];
    assert_eq!(summary["referencesVersion"], "references:v2");
    // 200 modules, each importing six modules of other packages: 1,200 file pairs (whose
    // inlined references exceed the frame) in 60 directory pairs, all in the overview.
    assert_eq!(summary["totals"]["productionEdges"], 1200, "{summary}");
    assert_eq!(summary["totals"]["productionSites"], 1200, "{summary}");
    assert_eq!(summary["counts"], refs_env["data"]["counts"]);
    let overview = &summary["overview"];
    assert_eq!(overview["total"], 60, "{overview}");
    assert_eq!(overview["omitted"], 0);
    let rows = overview["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 60);
    assert!(rows.iter().all(|row| row["sites"] == 20), "{overview}");
    assert_eq!(rows[0], json!({"source":"pkg0","target":"pkg1","sites":20}));
    for field in ["production", "tests", "unresolved", "filePairs", "packages"] {
        assert!(summary.get(field).is_none(), "{field}");
    }
    drop(raw);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #783: `payload.apps` follows the current rules bytes. A manifest a hidden layer selects
/// is left out; with no rules file every manifest is listed, and two apps add the per-app
/// instructions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn propose_rules_prompt_lists_apps_against_the_current_rules() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    for directory in ["src", "tools"] {
        fs::create_dir_all(temp.path().join("checkout").join(directory)).unwrap();
    }
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"svc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
            ),
            ("src/main.rs", "fn main() {}\n"),
            ("tools/package.json", "{\"name\":\"tooling\"}\n"),
            ("tools/build.js", "module.exports = 1;\n"),
        ],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    let rules_path = identity.root.join(".trellis/rules.toml");
    fs::write(
        &rules_path,
        "version = 1\n[layers.tools]\norder = 1\ndisplay = \"hidden\"\npaths = [\"tools/**\"]\n",
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(783, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 783, None);
    let mut prompt = |id: i64| {
        writeln!(
            client,
            "{}",
            modern(
                json!(id),
                "prompts/get",
                json!({"name":"trellis_propose_rules"})
            )
        )
        .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert!(reply.get("error").is_none(), "{reply}");
        let text = reply["result"]["messages"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .to_owned();
        let context: Value =
            serde_json::from_str(text.split("UNTRUSTED REPOSITORY DATA\n").nth(1).unwrap())
                .unwrap();
        (text, context["payload"]["apps"].clone())
    };
    let svc = json!({"name":"svc","source":"manifest","kind":"cargo","manifest":"Cargo.toml",
        "roots":["."],"entries":["svc"]});
    let (text, apps) = prompt(1);
    assert_eq!(apps, json!([svc]));
    assert!(!text.contains("PER-APP LAYERS"));
    fs::remove_file(&rules_path).unwrap();
    let (text, apps) = prompt(2);
    assert_eq!(
        apps,
        json!([svc, {"name":"tooling","source":"manifest","kind":"npm",
            "manifest":"tools/package.json","roots":["tools"],"entries":[]}])
    );
    assert!(text.contains("PER-APP LAYERS"));
    // #794: an authored app shadows the detected app at its root and is listed first, and
    // the instructions explain how to author apps.
    fs::write(
        &rules_path,
        "version = 1\n[apps.build-tools]\nroots = [\"tools\"]\n",
    )
    .unwrap();
    let (text, apps) = prompt(3);
    assert_eq!(
        apps,
        json!([{"name":"build-tools","source":"override","kind":null,"manifest":null,
            "roots":["tools"],"entries":[]}, svc])
    );
    assert!(text.contains("APPS\npayload.apps is the app inventory"));
    assert!(text.contains("[apps.backend]\nroots = [\".\"]"));
    drop(prompt);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn python_architecture_external_descriptors_validate_in_structured_tool_results() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[(
            "app.py",
            "import json as j\nfrom pathlib import Path\nimport os\nimport collections\nimport http.client\nimport winreg\n",
        )],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        r#"version = 1
[external_policy]
deny = [{ language = "python", package = "json", stdlib = true }]
[layers.source]
order = 1
paths = ["app.py"]
[layers.standard]
order = 2
external = [{ language = "python", package = "json", stdlib = true }]
"#,
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(163, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 163, None);
    let mut ask = |id, params| {
        writeln!(client, "{}", modern(json!(id), "tools/call", params)).unwrap();
        next_reply(&mut reader)
    };
    let refs = ask(
        1,
        json!({"name":"trellis_references","arguments":{"schemaVersion":1}}),
    );
    tool(&refs, None, true);
    let refs_env = &refs["result"]["structuredContent"];
    let descriptor = refs_env["data"]["externalPackages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["identity"]["package"] == "json")
        .unwrap();
    for name in ["json", "pathlib", "os", "collections", "http", "winreg"] {
        let item = refs_env["data"]["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["identity"]["package"] == name)
            .unwrap();
        assert_eq!(item["ecosystem"], "python");
        assert_eq!(
            item["evidence"],
            json!([{
                "kind":"standardLibraryCatalog", "catalog":"python-standard-library:v2", "entry":name
            }])
        );
    }
    let basis = refs_env["evidenceBasis"].clone();
    let checked = ask(
        2,
        json!({"name":"trellis_rules_check","arguments":{
            "schemaVersion":1,"expectedBasis":basis
        }}),
    );
    tool(&checked, None, true);
    let checked_env = &checked["result"]["structuredContent"];
    let instance = checked_env["data"]["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["rule"] == "$external")
        .unwrap();
    assert_eq!(instance["destination"], descriptor["identity"]);
    let explanation = ask(
        3,
        json!({"name":"trellis_rules_explain","arguments":{
            "schemaVersion":1,"expectedBasis":basis,"rule":"$external",
            "target":instance["target"],"destination":instance["destination"]
        }}),
    );
    tool(&explanation, None, true);
    validate_structured_content(&[
        json!({"name":"trellis_references","structuredContent":refs_env}),
        json!({"name":"trellis_rules_check","structuredContent":checked_env}),
        json!({"name":"trellis_rules_explain","structuredContent":explanation["result"]["structuredContent"]}),
    ]);
    drop(ask);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn javascript_architecture_external_descriptors_validate_in_structured_tool_results() {
    use trellis::daemon::registry::CheckoutRegistry;
    let temp = tempfile::tempdir().unwrap();
    let (identity, topology) = published_inactive_checkout(
        temp.path(),
        &[
            ("package.json", r#"{"dependencies":{"widget":"^1.2.3"}}"#),
            (
                "app.js",
                "import { send } from 'widget/sub';\nimport 'fs';\nimport 'node:fs/promises';\nimport 'path';\n",
            ),
        ],
    );
    fs::create_dir(identity.root.join(".trellis")).unwrap();
    fs::write(
        identity.root.join(".trellis/rules.toml"),
        r#"version = 1
[external_policy]
deny = [{ language = "javascript", package = "widget", stdlib = false }]
[layers.source]
order = 1
paths = ["app.js"]
[layers.standard]
order = 2
external = [{ language = "javascript", package = "widget", stdlib = false }]
"#,
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.attach_launch(163, &identity).unwrap();
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    architecture_built(&registry, &identity).await;
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 163, None);
    let mut ask = |id, params| {
        writeln!(client, "{}", modern(json!(id), "tools/call", params)).unwrap();
        next_reply(&mut reader)
    };
    let refs = ask(
        1,
        json!({"name":"trellis_references","arguments":{"schemaVersion":1}}),
    );
    tool(&refs, None, true);
    let refs_env = &refs["result"]["structuredContent"];
    let descriptor = refs_env["data"]["externalPackages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["identity"]["package"] == "widget")
        .unwrap();
    assert_eq!(descriptor["ecosystem"], "npm");
    assert_eq!(descriptor["evidence"][0]["path"], "package.json");
    assert_eq!(descriptor["evidence"][0]["importRoot"], "widget");
    assert_eq!(descriptor["evidence"][0]["versionConstraint"], "^1.2.3");
    for name in ["fs", "path"] {
        let item = refs_env["data"]["externalPackages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["identity"]["package"] == name)
            .unwrap();
        assert_eq!(item["ecosystem"], "node");
        assert_eq!(
            item["evidence"],
            json!([{
                "kind":"standardLibraryCatalog", "catalog":"node-standard-library:v1", "entry":name
            }])
        );
    }
    let basis = refs_env["evidenceBasis"].clone();
    let checked = ask(
        2,
        json!({"name":"trellis_rules_check","arguments":{
            "schemaVersion":1,"expectedBasis":basis
        }}),
    );
    tool(&checked, None, true);
    let checked_env = &checked["result"]["structuredContent"];
    let instance = checked_env["data"]["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["rule"] == "$external")
        .unwrap();
    assert_eq!(instance["destination"], descriptor["identity"]);
    let explanation = ask(
        3,
        json!({"name":"trellis_rules_explain","arguments":{
            "schemaVersion":1,"expectedBasis":basis,"rule":"$external",
            "target":instance["target"],"destination":instance["destination"]
        }}),
    );
    tool(&explanation, None, true);
    validate_structured_content(&[
        json!({"name":"trellis_references","structuredContent":refs_env}),
        json!({"name":"trellis_rules_check","structuredContent":checked_env}),
        json!({"name":"trellis_rules_explain","structuredContent":explanation["result"]["structuredContent"]}),
    ]);
    drop(ask);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn architecture_prompt_cancellation_deadline_and_final_root_are_not_tool_results() {
    for legacy in [false, true] {
        for scenario in ["cancel", "deadline", "root"] {
            let temp = tempfile::tempdir().unwrap();
            let (identity, registry, runtime) = ready_registry(temp.path(), 1290).await;
            let gate = ReadGate::default();
            runtime.set_mcp_admitted_gate_for_tests(gate.hook());
            let deadline = (scenario == "deadline").then_some(Duration::from_millis(300));
            let (mut client, mut reader, session) =
                socket_session(&identity, &registry, 1290, deadline);
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
                assert!(next_reply(&mut reader)["result"].is_object());
                writeln!(
                    client,
                    "{}",
                    json!({"jsonrpc":"2.0","method":"notifications/initialized"})
                )
                .unwrap();
            }
            let frame = |id: i64, method: &str, params: Value| {
                if legacy {
                    request(json!(id), method, params)
                } else {
                    modern(json!(id), method, params)
                }
            };
            writeln!(
                client,
                "{}",
                frame(1, "prompts/get", json!({"name":"trellis_propose_rules"}))
            )
            .unwrap();
            let waiter = gate.clone();
            tokio::task::spawn_blocking(move || waiter.await_held(1))
                .await
                .unwrap();
            match scenario {
                "cancel" => {
                    writeln!(client,"{}",json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}})).unwrap();
                    writeln!(client, "{}", frame(2, "prompts/list", json!({}))).unwrap();
                    assert_eq!(next_reply(&mut reader)["id"], 2);
                    gate.open();
                    drop(reader);
                    drop(client);
                    session.join().unwrap().unwrap();
                }
                "deadline" => {
                    let reply = next_reply(&mut reader);
                    gate.open();
                    assert_eq!(
                        reply["error"]["data"]["code"], "deadline_exceeded",
                        "{reply}"
                    );
                    assert!(reply.get("result").is_none());
                    assert!(reply["error"]["data"].get("attemptedWorkspace").is_none());
                    drop(reader);
                    drop(client);
                    session.join().unwrap().unwrap();
                }
                "root" => {
                    let moved = temp.path().join("moved-checkout");
                    fs::rename(&identity.root, &moved).unwrap();
                    fs::create_dir(&identity.root).unwrap();
                    gate.open();
                    let reply = next_reply(&mut reader);
                    assert_eq!(reply["error"]["data"]["code"], "root_changed", "{reply}");
                    assert!(reply.get("result").is_none());
                    drop(reader);
                    drop(client);
                    session.join().unwrap().unwrap();
                }
                _ => unreachable!(),
            }
        }
    }
}

// Search integration uses a real native publication, with startup paused so
// coverage overrides and retained-head guards cannot race reconciliation.
struct SearchMcpFixture {
    _temp: tempfile::TempDir,
    identity: trellis::store::topology::WorkspaceIdentity,
    registry: std::sync::Arc<tokio::sync::Mutex<trellis::daemon::registry::CheckoutRegistry>>,
    startup: ReadGate,
    previous: trellis::model::IndexPin,
    current: trellis::model::IndexPin,
    vectors: PathBuf,
}

impl SearchMcpFixture {
    async fn new(files: &[(&str, &str)], incomplete_python: bool) -> Self {
        Self::with_optional_search(files, incomplete_python, |_| None, false)
            .await
            .0
    }

    async fn with_optional_search(
        files: &[(&str, &str)],
        incomplete_python: bool,
        make_service: impl FnOnce(
            &trellis::store::topology::TopologyRoots,
        )
            -> Option<std::sync::Arc<trellis::daemon::search::SearchService>>,
        start_fill: bool,
    ) -> (
        Self,
        Option<std::sync::Arc<trellis::daemon::search::SearchService>>,
        Option<std::sync::Arc<trellis::daemon::search_fill::SearchFill>>,
    ) {
        Self::with_optional_search_limit(files, incomplete_python, make_service, start_fill, None)
            .await
    }

    async fn with_optional_search_limit(
        files: &[(&str, &str)],
        incomplete_python: bool,
        make_service: impl FnOnce(
            &trellis::store::topology::TopologyRoots,
        )
            -> Option<std::sync::Arc<trellis::daemon::search::SearchService>>,
        start_fill: bool,
        matrix_limit: Option<usize>,
    ) -> (
        Self,
        Option<std::sync::Arc<trellis::daemon::search::SearchService>>,
        Option<std::sync::Arc<trellis::daemon::search_fill::SearchFill>>,
    ) {
        let temp = tempfile::tempdir().unwrap();
        let (identity, topology) = published_inactive_checkout(temp.path(), files);
        let vectors = topology.vectors_db();
        let store = trellis::testing::retry_admission(|| {
            trellis::store::Store::open_for_tests(temp.path(), &identity.root)
        });
        let previous = store.index_baseline().unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (graph, native, capture) = trellis::indexer::index_workspace_bundle(
            &trellis::indexer::IndexOptions::new(identity.root.clone()),
            store.root_id(),
            &cancel,
            |_| {},
        )
        .unwrap();
        let writer = store.publication_for_tests().unwrap();
        let current = store
            .publish_native(&graph, &capture, &native, &writer, previous, &cancel)
            .unwrap();
        assert!(current.index_revision > previous.index_revision);
        if incomplete_python {
            store
                .mcp_set_coverage_for_tests(&writer, "b.py", true, "partial")
                .unwrap();
        }
        drop(writer);
        drop(store);
        let service = make_service(&topology);
        let fill = start_fill.then(|| {
            let service = service.as_ref().unwrap().clone();
            match matrix_limit {
                Some(limit) => {
                    trellis::daemon::search_fill::SearchFill::start_with_limit_for_tests(
                        service, limit,
                    )
                }
                None => trellis::daemon::search_fill::SearchFill::start(service),
            }
            .unwrap()
        });
        let mut registry = trellis::daemon::registry::CheckoutRegistry::with_roots(topology);
        if let Some(service) = &service {
            registry.set_search_service(service.clone());
        }
        if let Some(fill) = &fill {
            registry.set_search_fill(fill.clone());
        }
        registry.attach_launch(211, &identity).unwrap();
        let startup = ReadGate::default();
        let hold = startup.hook();
        registry.set_next_startup_hook_for_tests(hold);
        registry.activate(&identity.root_key).unwrap();
        let waiter = startup.clone();
        tokio::task::spawn_blocking(move || waiter.await_held(1))
            .await
            .unwrap();
        (
            Self {
                _temp: temp,
                identity,
                registry: std::sync::Arc::new(tokio::sync::Mutex::new(registry)),
                startup,
                previous,
                current,
                vectors,
            },
            service,
            fill,
        )
    }

    async fn head(&self) -> std::sync::Arc<trellis::daemon::search_head::HeadSlot> {
        self.registry
            .lock()
            .await
            .runtime(&self.identity.root_key)
            .unwrap()
            .browser_scheduler()
            .unwrap()
            .search_head_slot_for_tests()
            .unwrap()
    }

    async fn prewarm(&self) {
        let runtime = self
            .registry
            .lock()
            .await
            .runtime(&self.identity.root_key)
            .unwrap();
        self.startup.open();
        let waiter = runtime.clone();
        tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
            loop {
                let seen = waiter.state_version();
                if waiter.browser_scheduler().unwrap().runtime_phase()
                    == trellis::http::RuntimePhase::Ready
                {
                    break;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                assert!(
                    !remaining.is_zero(),
                    "startup reconciliation did not settle"
                );
                waiter.wait_state_change(seen, remaining);
            }
        })
        .await
        .unwrap();
        let head = self.head().await;
        tokio::task::spawn_blocking(move || {
            let budget = trellis::search_embed::QueryBudget::new(
                std::time::Instant::now() + trellis::testing::HANG_BACKSTOP,
                std::sync::Arc::new(std::time::Instant::now),
            );
            head.request();
            head.wait_latest(&std::sync::atomic::AtomicBool::new(false), &budget)
                .unwrap();
        })
        .await
        .unwrap();
    }

    fn seed_vectors(&self, service: &trellis::daemon::search::SearchService) {
        self.seed_vectors_at(service, self.current);
    }

    /// Cache a vector for every head card at `pin`.
    fn seed_vectors_at(
        &self,
        service: &trellis::daemon::search::SearchService,
        pin: trellis::model::IndexPin,
    ) {
        let store = trellis::testing::retry_admission(|| {
            trellis::store::Store::open_for_tests(self._temp.path(), &self.identity.root)
        });
        let mut cards = Vec::new();
        let mut after = None;
        loop {
            let page = store
                .head_search_cards(Some(pin), after.as_deref(), 16)
                .unwrap();
            if page.cards.is_empty() {
                break;
            }
            after = Some(page.cards.last().unwrap().syntax_id.clone());
            cards.extend(page.cards);
        }
        drop(store);
        let model_id = service
            .with_model_job(&std::sync::atomic::AtomicBool::new(false), |_| {
                Ok(service
                    .status_handle()
                    .snapshot(
                        trellis::search_card::CardFormat::V1.as_str(),
                        0,
                        0,
                        trellis::search_model::LexicalState::Complete,
                        false,
                    )
                    .model_id
                    .unwrap())
            })
            .unwrap();
        service
            .with_vector_writer(|writer| {
                for card in cards {
                    writer.put(&model_id, &card.card_hash, &[1; 1024])?;
                }
                Ok(())
            })
            .unwrap();
    }

    fn basis(pin: trellis::model::IndexPin) -> Value {
        json!({"indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision})
    }
}

impl Drop for SearchMcpFixture {
    fn drop(&mut self) {
        self.startup.open();
    }
}

fn search_socket_ask(
    client: &mut std::os::unix::net::UnixStream,
    reader: &mut BufReader<std::os::unix::net::UnixStream>,
    legacy: bool,
    id: u64,
    arguments: Value,
) -> Value {
    writeln!(
        client,
        "{}",
        call(legacy, json!(id), "trellis_search_symbols", arguments)
    )
    .unwrap();
    let reply = next_reply(reader);
    assert_eq!(reply["id"], id);
    let mut frame = serde_json::to_vec(&reply).unwrap();
    frame.push(b'\n');
    assert_reply_cap(&frame);
    reply
}

fn search_socket_initialize(
    client: &mut std::os::unix::net::UnixStream,
    reader: &mut BufReader<std::os::unix::net::UnixStream>,
    legacy: bool,
) {
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
            next_reply(reader)["result"]["protocolVersion"],
            "2025-11-25"
        );
        writeln!(
            client,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
    }
}

fn search_schema_pair(reply: &Value) -> Value {
    json!({"name":"trellis_search_symbols","structuredContent":reply["result"]["structuredContent"]})
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_both_modes_use_real_lexical_cards_without_optional_cache() {
    let source: String = (0..51)
        .map(|index| format!("function orchard{index:03}() {{}}\n"))
        .collect();
    let fixture = SearchMcpFixture::new(&[("a.js", &source)], false).await;
    let mut schema_pairs = Vec::new();
    for legacy in [false, true] {
        let (mut client, mut reader, session) =
            socket_session(&fixture.identity, &fixture.registry, 211, None);
        search_socket_initialize(&mut client, &mut reader, legacy);
        for (id, limit, count) in [(1, None, 10), (2, Some(50), 50), (3, Some(1), 1)] {
            let mut args = json!({"schemaVersion":1,"query":"orchard"});
            if let Some(limit) = limit {
                args["limit"] = json!(limit);
            }
            let reply = search_socket_ask(&mut client, &mut reader, legacy, id, args);
            tool(&reply, None, !legacy);
            let envelope = &reply["result"]["structuredContent"];
            assert_eq!(
                envelope["evidenceBasis"],
                SearchMcpFixture::basis(fixture.current)
            );
            assert_eq!(envelope["data"]["role"], "any");
            let hits = envelope["data"]["hits"].as_array().unwrap();
            assert_eq!(hits.len(), count, "{reply}");
            for (index, hit) in hits.iter().enumerate() {
                assert_eq!(hit["rank"], index + 1);
                assert_eq!(hit["matchedBy"], json!(["lexical"]));
                assert_eq!(hit["evidenceTier"], "syntax");
                assert_eq!(
                    emitted_document(&envelope["data"], hit)["document"]["path"],
                    "a.js"
                );
            }
            assert_eq!(envelope["truncated"], true);
            assert_eq!(envelope["truncationReason"], "limit");
            assert_eq!(
                envelope["data"]["searchIndex"],
                json!({
                    "state":"unavailable","modelId":null,"modelName":null,"backend":null,
                    "declarations":51,"embedded":0,"lexical":"complete",
                    "unavailableReason":"model_not_configured"
                })
            );
            assert_eq!(envelope["warnings"].as_array().unwrap().len(), 1);
            assert_eq!(envelope["warnings"][0]["code"], "syntaxOnly");
            schema_pairs.push(search_schema_pair(&reply));
        }
        for (id, query) in [(4, "__ * : ( )"), (5, "the and for")] {
            let reply = search_socket_ask(
                &mut client,
                &mut reader,
                legacy,
                id,
                json!({"schemaVersion":1,"query":query}),
            );
            tool(&reply, None, !legacy);
            let envelope = &reply["result"]["structuredContent"];
            for field in ["hits", "documents", "coverage", "provenance"] {
                assert_eq!(envelope["data"][field], json!([]));
            }
            assert_eq!(envelope["partial"], false);
            assert_eq!(envelope["truncated"], false);
            assert_eq!(envelope["data"]["searchIndex"]["lexical"], "complete");
            assert_eq!(envelope["data"]["role"], "any");
            schema_pairs.push(search_schema_pair(&reply));
        }
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
    assert!(
        !fixture.vectors.exists(),
        "search created an optional cache"
    );
    for suffix in ["-wal", "-shm", "-journal"] {
        assert!(!PathBuf::from(format!("{}{suffix}", fixture.vectors.display())).exists());
    }
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_model_configuration_reasons_reach_describe_and_status() {
    use trellis::{
        daemon::search::SearchService, search_embed::EmbedError, search_model::SearchConfig,
    };
    for scenario in ["unconfigured", "load_failed", "managed_missing"] {
        let configured = scenario == "load_failed";
        let home = tempfile::tempdir().unwrap();
        let config_file = home.path().join(".config/trellis/config.toml");
        let models_dir = home.path().join("custom-models");
        let mut expected_status = json!({
            "backend":"cpu",
            "unavailableReason":if configured { "model_load_failed" } else { "model_not_configured" },
            "state":"unavailable","declarations":1,"embedded":0
        });
        if scenario == "managed_missing" {
            fs::create_dir_all(config_file.parent().unwrap()).unwrap();
            fs::write(
                &config_file,
                format!(
                    "models_dir = {:?}\nbackend = \"cpu\"\n[model]\nname = {:?}\nrevision = {:?}\n",
                    models_dir.to_str().unwrap(),
                    REVIEWED_MODEL_NAME,
                    REVIEWED_MODEL_REVISION,
                ),
            )
            .unwrap();
            expected_status["configuration"] = json!({
                "configFile":config_file,
                "modelsDir":models_dir,
                "modelName":REVIEWED_MODEL_NAME,
                "modelRevision":REVIEWED_MODEL_REVISION,
                "modelPath":models_dir
                    .join(REVIEWED_MODEL_NAME)
                    .join(REVIEWED_MODEL_REVISION),
                "backend":"cpu",
                "sources":{"modelsDir":"config_file","model":"config_file","backend":"config_file"}
            });
        }
        let (fixture, service, _) = SearchMcpFixture::with_optional_search(
            &[("a.js", "function orchard() {}\n")],
            false,
            |roots| {
                Some(std::sync::Arc::new(if scenario == "managed_missing" {
                    let resolved = trellis::search_config::ResolvedSearchConfig::from_inputs(
                        config_file.clone(),
                        roots.data.join("models"),
                        None,
                        None,
                    )
                    .unwrap();
                    assert!(resolved.runtime_config().unwrap().model_path().is_none());
                    SearchService::from_resolved(roots.clone(), resolved).unwrap()
                } else {
                    SearchService::new(
                        roots.clone(),
                        SearchConfig::from_values(
                            configured.then(|| roots.cache.join("missing-model")),
                            Some("cpu"),
                        )
                        .unwrap(),
                    )
                    .unwrap()
                }))
            },
            false,
        )
        .await;
        let service = service.unwrap();
        if configured {
            let result: Result<(), EmbedError> = service.with_model_job(
                &std::sync::atomic::AtomicBool::new(false),
                |_| unreachable!(),
            );
            assert!(matches!(result, Err(EmbedError::ModelUnavailable(_))));
        }
        let reason = if configured {
            "model_load_failed"
        } else {
            "model_not_configured"
        };
        let runtime = fixture
            .registry
            .lock()
            .await
            .runtime(&fixture.identity.root_key)
            .unwrap();
        let cli_status = runtime.cli_status().unwrap();
        assert_eq!(cli_status["searchIndex"], expected_status);
        let mut pairs = Vec::new();
        for legacy in [false, true] {
            let (mut client, mut reader, session) =
                socket_session(&fixture.identity, &fixture.registry, 211, None);
            search_socket_initialize(&mut client, &mut reader, legacy);
            let reply = search_socket_ask(
                &mut client,
                &mut reader,
                legacy,
                1,
                json!({"schemaVersion":1,"query":"orchard"}),
            );
            let envelope = &reply["result"]["structuredContent"];
            assert_eq!(
                envelope["data"]["searchIndex"]["unavailableReason"], reason,
                "{reply}"
            );
            assert_eq!(envelope["data"]["hits"][0]["matchedBy"], json!(["lexical"]));
            assert!(
                envelope["data"]["searchIndex"]
                    .get("configuration")
                    .is_none()
            );
            assert_eq!(envelope["data"]["searchIndex"]["embedded"], 0);
            assert!(envelope["data"]["searchIndex"]["modelId"].is_null());
            pairs.push(search_schema_pair(&reply));
            writeln!(
                client,
                "{}",
                call(
                    legacy,
                    json!(2),
                    "trellis_workspace_describe",
                    json!({"schemaVersion":1})
                )
            )
            .unwrap();
            let describe = next_reply(&mut reader);
            assert_eq!(
                describe["result"]["structuredContent"]["data"]["searchIndex"],
                cli_status["searchIndex"],
                "{describe}"
            );
            pairs.push(json!({"name":"trellis_workspace_describe","structuredContent":describe["result"]["structuredContent"]}));
            drop(reader);
            drop(client);
            session.join().unwrap().unwrap();
        }
        assert!(!fixture.vectors.exists());
        assert!(!models_dir.exists());
        validate_structured_content(&pairs);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_role_filters_stored_inline_roles_before_limit_and_keeps_exact_names() {
    // Both roles share a normal path: the test role comes from the inline marker.
    let source = r#"
pub fn read_status_for() {}
#[cfg(test)]
mod inline {
    pub fn read_status_for() {}
    #[test]
    fn read_status_for_is_observed() {}
}
"#;
    let fixture = SearchMcpFixture::new(&[("a.rs", source)], false).await;
    let mut schema_pairs = Vec::new();
    for legacy in [false, true] {
        let (mut client, mut reader, session) =
            socket_session(&fixture.identity, &fixture.registry, 211, None);
        search_socket_initialize(&mut client, &mut reader, legacy);
        let mut declarations = None;
        for (index, role) in [None, Some("any"), Some("production"), Some("test")]
            .into_iter()
            .enumerate()
        {
            let mut args = json!({"schemaVersion":1,"query":"read_status_for","limit":1});
            if let Some(role) = role {
                args["role"] = json!(role);
            }
            let reply = search_socket_ask(&mut client, &mut reader, legacy, 1 + index as u64, args);
            tool(&reply, None, !legacy);
            let envelope = &reply["result"]["structuredContent"];
            let data = &envelope["data"];
            assert_eq!(data["role"], role.unwrap_or("any"));
            let hits = data["hits"].as_array().unwrap();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0]["name"], "read_status_for");
            assert_eq!(hits[0]["rank"], 1);
            assert_eq!(hits[0]["matchedBy"], json!(["lexical"]));
            let expected_role = if role == Some("test") {
                "test"
            } else {
                "production"
            };
            assert_eq!(hits[0]["role"], expected_role);
            assert_eq!(emitted_document(data, &hits[0])["document"]["path"], "a.rs");
            // Filtering is a retrieval choice, not a smaller published head/status.
            if let Some(expected) = &declarations {
                assert_eq!(&data["searchIndex"]["declarations"], expected);
            } else {
                declarations = Some(data["searchIndex"]["declarations"].clone());
            }
            let declaration_args = json!({"schemaVersion":1,"symbolId":hits[0]["symbolId"],
                "expectedBasis":envelope["evidenceBasis"]});
            schema_pairs.push(search_schema_pair(&reply));
            writeln!(
                client,
                "{}",
                call(
                    legacy,
                    json!(10 + index),
                    "trellis_declaration",
                    declaration_args
                )
            )
            .unwrap();
            let declaration = next_reply(&mut reader);
            tool(&declaration, None, !legacy);
            assert_eq!(
                declaration["result"]["structuredContent"]["data"]["declaration"]["name"],
                "read_status_for"
            );
            assert_eq!(
                declaration["result"]["structuredContent"]["data"]["declaration"]["role"],
                expected_role
            );
            schema_pairs.push(json!({"name":"trellis_declaration",
                "structuredContent":declaration["result"]["structuredContent"]}));
        }
        let reply = search_socket_ask(
            &mut client,
            &mut reader,
            legacy,
            20,
            json!({"schemaVersion":1,"query":"read_status_for","role":"any","limit":50}),
        );
        tool(&reply, None, !legacy);
        let hits = reply["result"]["structuredContent"]["data"]["hits"]
            .as_array()
            .unwrap();
        assert!(
            hits.len() >= 3,
            "exact retrieval must keep nonexact matches: {reply}"
        );
        assert_eq!(hits[0]["name"], "read_status_for");
        assert_eq!(hits[0]["role"], "production");
        assert_eq!(hits[1]["name"], "read_status_for");
        assert_eq!(hits[1]["role"], "test");
        assert!(
            hits.iter()
                .any(|hit| hit["name"] == "read_status_for_is_observed")
        );
        schema_pairs.push(search_schema_pair(&reply));
        // The existing find tool does not acquire a role parameter or echo field.
        for (id, role) in [(21, None), (22, Some("test"))] {
            let mut args = json!({"schemaVersion":1,"query":"read_status_for"});
            if let Some(role) = role {
                args["role"] = json!(role);
            }
            writeln!(
                client,
                "{}",
                call(legacy, json!(id), "trellis_find_symbols", args)
            )
            .unwrap();
            let found = next_reply(&mut reader);
            tool(&found, role.map(|_| "invalid_request"), !legacy);
            if role.is_none() {
                assert!(
                    found["result"]["structuredContent"]["data"]
                        .get("role")
                        .is_none()
                );
            }
            schema_pairs.push(json!({"name":"trellis_find_symbols",
                "structuredContent":found["result"]["structuredContent"]}));
        }
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
    assert!(!fixture.vectors.exists());
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_closed_inputs_and_utf8_query_byte_boundaries() {
    let fixture = SearchMcpFixture::new(&[("a.js", "function café() {}\n")], false).await;
    let mut schema_pairs = Vec::new();
    for legacy in [false, true] {
        let (mut client, mut reader, session) =
            socket_session(&fixture.identity, &fixture.registry, 211, None);
        search_socket_initialize(&mut client, &mut reader, legacy);
        let boundary = "é".repeat(512);
        assert_eq!(boundary.len(), 1024);
        for (id, query) in [(1, boundary.as_str()), (2, "CAFÉ"), (3, " café ")] {
            let reply = search_socket_ask(
                &mut client,
                &mut reader,
                legacy,
                id,
                json!({"schemaVersion":1,"query":query,"limit":50}),
            );
            tool(&reply, None, !legacy);
            if id != 1 {
                assert_eq!(
                    reply["result"]["structuredContent"]["data"]["hits"][0]["name"],
                    "café"
                );
            }
            schema_pairs.push(search_schema_pair(&reply));
        }
        let invalid = [
            (json!({"schemaVersion":1,"query":""}), "invalid_request"),
            (json!({"schemaVersion":1,"query":null}), "invalid_request"),
            (
                json!({"schemaVersion":1,"query":format!("{boundary}a")}),
                "invalid_request",
            ),
            (
                json!({"schemaVersion":1,"query":"café","limit":0}),
                "range_too_large",
            ),
            (
                json!({"schemaVersion":1,"query":"café","limit":51}),
                "range_too_large",
            ),
            (
                json!({"schemaVersion":1,"query":"café","limit":null}),
                "invalid_request",
            ),
            (
                json!({"schemaVersion":1,"query":"café","expectedBasis":null}),
                "invalid_request",
            ),
            (
                json!({"schemaVersion":1,"query":"café","expectedBasis":{"indexRevision":1}}),
                "invalid_request",
            ),
            (
                json!({"schemaVersion":1,"query":"café","backend":"cpu"}),
                "invalid_request",
            ),
            (
                json!({"schemaVersion":1,"query":"café","score":true}),
                "invalid_request",
            ),
            (
                json!({"schemaVersion":1,"query":"café","filters":{}}),
                "invalid_request",
            ),
            (
                json!({"schemaVersion":1,"query":"café","view":"search"}),
                "invalid_request",
            ),
        ];
        for (index, (arguments, code)) in invalid.into_iter().enumerate() {
            let reply = search_socket_ask(
                &mut client,
                &mut reader,
                legacy,
                10 + index as u64,
                arguments,
            );
            tool(&reply, Some(code), !legacy);
            schema_pairs.push(search_schema_pair(&reply));
        }
        for (index, role) in [
            json!(""),
            json!("Any"),
            json!("tests"),
            json!(" production"),
            json!("test "),
            json!(null),
            json!(true),
            json!(1),
            json!([]),
            json!({}),
        ]
        .into_iter()
        .enumerate()
        {
            let reply = search_socket_ask(
                &mut client,
                &mut reader,
                legacy,
                30 + index as u64,
                json!({"schemaVersion":1,"query":"café","role":role}),
            );
            tool(&reply, Some("invalid_request"), !legacy);
            schema_pairs.push(search_schema_pair(&reply));
        }
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
    assert!(!fixture.vectors.exists());
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_accepts_current_basis_but_refuses_retained_and_other_generations() {
    let fixture = SearchMcpFixture::new(&[("a.js", "function orchard() {}\n")], false).await;
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let current = SearchMcpFixture::basis(fixture.current);
    let accepted = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        1,
        json!({"schemaVersion":1,"query":"orchard","expectedBasis":current}),
    );
    tool(&accepted, None, true);
    assert_eq!(
        accepted["result"]["structuredContent"]["evidenceBasis"],
        current
    );
    let mut wrong_generation = current.clone();
    wrong_generation["indexGeneration"] = json!("123e4567-e89b-42d3-a456-426614174000");
    assert_ne!(wrong_generation, current);
    let mut stale = current.clone();
    stale["indexRevision"] = json!(0);
    let mut schema_pairs = vec![search_schema_pair(&accepted)];
    for (index, pin) in [
        SearchMcpFixture::basis(fixture.previous),
        wrong_generation,
        stale,
    ]
    .into_iter()
    .enumerate()
    {
        let reply = search_socket_ask(
            &mut client,
            &mut reader,
            false,
            index as u64 + 2,
            json!({"schemaVersion":1,"query":"zzzzNoMatch","expectedBasis":pin}),
        );
        tool(&reply, Some("revision_conflict"), true);
        let envelope = &reply["result"]["structuredContent"];
        assert_eq!(envelope["error"]["currentBasis"], current);
        assert!(envelope.get("data").is_none());
        schema_pairs.push(search_schema_pair(&reply));
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_zero_hits_keep_incomplete_native_scope_separate_from_vectors() {
    let fixture = SearchMcpFixture::new(
        &[
            ("a.js", "function orchard() {}\n"),
            ("b.py", "def unrelated(): pass\n"),
        ],
        true,
    )
    .await;
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let mut schema_pairs = Vec::new();
    for (id, query, count) in [(1, "zzzzNoMatch", 0), (2, "orchard", 1)] {
        let reply = search_socket_ask(
            &mut client,
            &mut reader,
            false,
            id,
            json!({"schemaVersion":1,"query":query}),
        );
        tool(&reply, None, true);
        let envelope = &reply["result"]["structuredContent"];
        assert_eq!(envelope["data"]["hits"].as_array().unwrap().len(), count);
        assert_eq!(envelope["partial"], true);
        assert_eq!(envelope["truncated"], false);
        assert_eq!(envelope["warnings"][0]["code"], "coverageIncomplete");
        assert_eq!(envelope["warnings"][1]["code"], "syntaxOnly");
        assert_eq!(envelope["warnings"].as_array().unwrap().len(), 2);
        // #715: the hit's language only; with no hit, the searched languages. The
        // incomplete Python scope keeps the answer partial either way.
        let scope = envelope["data"]["scopeCoverage"].as_array().unwrap();
        let languages: Vec<_> = scope
            .iter()
            .map(|row| row["language"].as_str().unwrap())
            .collect();
        if count == 0 {
            assert_eq!(languages, ["python", "javascript"]);
            assert_eq!(scope[0]["coverage"]["partial"], 1);
        } else {
            assert_eq!(languages, ["javascript"]);
        }
        assert_eq!(envelope["data"]["searchIndex"]["state"], "unavailable");
        assert_eq!(envelope["data"]["searchIndex"]["embedded"], 0);
        if count == 0 {
            for field in ["documents", "coverage", "coverageDiagnostics", "provenance"] {
                assert_eq!(envelope["data"][field], json!([]));
            }
        } else {
            let coverage = envelope["data"]["coverage"].as_array().unwrap();
            assert_eq!(coverage.len(), 1);
            assert_eq!(coverage[0]["documentPath"], "a.js");
        }
        schema_pairs.push(search_schema_pair(&reply));
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_keeps_admitted_head_and_followup_pin_conflicts_after_publication() {
    let temp = tempfile::tempdir().unwrap();
    let (identity, registry, runtime) = ready_registry(temp.path(), 211).await;
    let scheduler = runtime.browser_scheduler().unwrap();
    let worker_gate = ReadGate::default();
    let gate = ReadGate::default();
    let _release_on_failure = McpFillRelease([worker_gate.clone(), gate.clone()]);
    let held = worker_gate.clone();
    let worker_scheduler = scheduler.clone();
    let worker = tokio::task::spawn_blocking(move || {
        let _worker = worker_scheduler.hold_worker_for_tests();
        (held.hook())();
    });
    let held = worker_gate.clone();
    tokio::task::spawn_blocking(move || held.await_held(1))
        .await
        .unwrap();
    let admitted = {
        let (response, _) = runtime.evidence_response().unwrap();
        let pin = response.status().unwrap().revision;
        response.finish(()).unwrap();
        pin
    };
    runtime.set_mcp_admitted_gate_for_tests(gate.hook());
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 211, None);
    writeln!(client, "{}", call(false, json!(1), "trellis_search_symbols",
        json!({"schemaVersion":1,"query":"held","expectedBasis":SearchMcpFixture::basis(admitted)}))).unwrap();
    let waiter = gate.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    fs::write(identity.root.join("a.js"), "function moved() {}\n").unwrap();
    let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
        notify::event::DataChange::Content,
    )))
    .add_path(identity.root.join("a.js"));
    assert!(scheduler.submit_watch_event_for_tests(Ok(event)));
    assert!(scheduler.checkout_watch_pending());
    gate.open();
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(envelope["evidenceBasis"], SearchMcpFixture::basis(admitted));
    assert_eq!(envelope["data"]["hits"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["data"]["hits"][0]["name"], "held");
    let symbol_id = envelope["data"]["hits"][0]["symbolId"].clone();
    worker_gate.open();
    worker.await.unwrap();
    tokio::time::timeout(trellis::testing::HANG_BACKSTOP, async {
        loop {
            let (response, _) = runtime.evidence_response().unwrap();
            let pin = response.status().unwrap().revision;
            response.finish(()).unwrap();
            if pin != admitted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("edit was not published");
    let conflict = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        2,
        json!({"schemaVersion":1,"query":"held","expectedBasis":SearchMcpFixture::basis(admitted)}),
    );
    tool(&conflict, Some("revision_conflict"), true);
    writeln!(client, "{}", call(false, json!(3), "trellis_declaration", json!({
        "schemaVersion":1,"symbolId":symbol_id,"expectedBasis":SearchMcpFixture::basis(admitted)
    }))).unwrap();
    tool(&next_reply(&mut reader), Some("revision_conflict"), true);
    let newer = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        4,
        json!({"schemaVersion":1,"query":"moved"}),
    );
    tool(&newer, None, true);
    assert_eq!(
        newer["result"]["structuredContent"]["data"]["hits"][0]["name"],
        "moved"
    );
    validate_structured_content(&[
        search_schema_pair(&reply),
        search_schema_pair(&conflict),
        search_schema_pair(&newer),
    ]);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

impl SearchMcpFixture {
    /// Install the daemon search service before activation; the startup gate
    /// keeps publication and optional fill work out of test setup.
    async fn with_search(
        files: &[(&str, &str)],
        make_service: impl FnOnce(
            &trellis::store::topology::TopologyRoots,
        ) -> std::sync::Arc<trellis::daemon::search::SearchService>,
        start_fill: bool,
    ) -> (
        Self,
        std::sync::Arc<trellis::daemon::search::SearchService>,
        Option<std::sync::Arc<trellis::daemon::search_fill::SearchFill>>,
    ) {
        let (fixture, service, fill) =
            Self::with_optional_search(files, false, |roots| Some(make_service(roots)), start_fill)
                .await;
        (fixture, service.unwrap(), fill)
    }
}

fn install_unusable_search_cache(path: &Path, stale_format: bool) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    // No vector reader or writer has opened this path. In particular, never
    // close a raw file descriptor beneath a live SQLite connection.
    if stale_format {
        let db = rusqlite::Connection::open(path).unwrap();
        db.pragma_update(None, "user_version", 2).unwrap();
    } else {
        fs::write(path, b"not a SQLite database").unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_without_model_ignores_stale_and_corrupt_optional_cache() {
    let mut schema_pairs = Vec::new();
    for configured_service in [false, true] {
        for stale_format in [true, false] {
            let fixture = if configured_service {
                SearchMcpFixture::with_search(
                    &[("a.js", "function orchard() {}\n")],
                    |roots| {
                        std::sync::Arc::new(
                            trellis::daemon::search::SearchService::new(
                                roots.clone(),
                                trellis::search_model::SearchConfig::from_values(None, Some("cpu"))
                                    .unwrap(),
                            )
                            .unwrap(),
                        )
                    },
                    false,
                )
                .await
                .0
            } else {
                SearchMcpFixture::new(&[("a.js", "function orchard() {}\n")], false).await
            };
            install_unusable_search_cache(&fixture.vectors, stale_format);
            let (mut client, mut reader, session) =
                socket_session(&fixture.identity, &fixture.registry, 211, None);
            let reply = search_socket_ask(
                &mut client,
                &mut reader,
                false,
                1,
                json!({"schemaVersion":1,"query":"orchard"}),
            );
            tool(&reply, None, true);
            let envelope = &reply["result"]["structuredContent"];
            assert_eq!(
                envelope["evidenceBasis"],
                SearchMcpFixture::basis(fixture.current)
            );
            assert_eq!(envelope["data"]["hits"].as_array().unwrap().len(), 1);
            assert_eq!(envelope["data"]["hits"][0]["name"], "orchard");
            assert_eq!(envelope["data"]["hits"][0]["matchedBy"], json!(["lexical"]));
            let backend = if configured_service {
                json!("cpu")
            } else {
                Value::Null
            };
            assert_eq!(
                envelope["data"]["searchIndex"],
                json!({
                    "state":"unavailable", "modelId":null, "modelName":null,
                    "backend":backend, "declarations":1, "embedded":0, "lexical":"complete",
                    "unavailableReason":"model_not_configured"
                })
            );
            assert_eq!(envelope["partial"], false);
            schema_pairs.push(search_schema_pair(&reply));
            drop(reader);
            drop(client);
            session.join().unwrap().unwrap();
        }
    }
    validate_structured_content(&schema_pairs);
}

#[derive(Clone, Default)]
struct McpSearchEmbedderFactory {
    loads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl trellis::search_embed::SearchEmbedderFactory for McpSearchEmbedderFactory {
    fn load(
        &self,
        control: &trellis::search_embed::EmbeddingControl<'_>,
    ) -> Result<Box<dyn trellis::search_embed::SearchEmbedder>, trellis::search_embed::EmbedError>
    {
        control.check()?;
        self.loads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(Box::new(McpSearchEmbedder))
    }
}

struct McpSearchEmbedder;
impl trellis::search_embed::SearchEmbedder for McpSearchEmbedder {
    fn embed(
        &self,
        texts: &[String],
        _kind: trellis::search_embed::InputKind,
        control: &trellis::search_embed::EmbeddingControl<'_>,
    ) -> Result<Vec<trellis::search_embed::QuantizedVector>, trellis::search_embed::EmbedError>
    {
        control.check()?;
        Ok(texts
            .iter()
            .map(|_| trellis::search_embed::QuantizedVector {
                values: vec![1; 1024],
            })
            .collect())
    }
}

#[derive(Clone)]
struct McpBudgetEmbedderFactory {
    inference: ReadGate,
}

impl trellis::search_embed::SearchEmbedderFactory for McpBudgetEmbedderFactory {
    fn load(
        &self,
        control: &trellis::search_embed::EmbeddingControl<'_>,
    ) -> Result<Box<dyn trellis::search_embed::SearchEmbedder>, trellis::search_embed::EmbedError>
    {
        control.check()?;
        Ok(Box::new(self.clone()))
    }
}

impl trellis::search_embed::SearchEmbedder for McpBudgetEmbedderFactory {
    fn embed(
        &self,
        texts: &[String],
        kind: trellis::search_embed::InputKind,
        control: &trellis::search_embed::EmbeddingControl<'_>,
    ) -> Result<Vec<trellis::search_embed::QuantizedVector>, trellis::search_embed::EmbedError>
    {
        control.check()?;
        assert_eq!(kind, trellis::search_embed::InputKind::Query);
        // Model work has begun within budget. The next cooperative boundary
        // observes the request's advanced clock, not wall-clock elapsed time.
        (self.inference.hook())();
        control.check()?;
        McpSearchEmbedder.embed(texts, kind, control)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_inference_budget_returns_complete_lexical_and_keeps_required_failures() {
    let source: String = (0..17)
        .map(|index| format!("function orchard{index:02}() {{}}\n"))
        .collect();
    let mut schema_pairs = Vec::new();
    for scenario in ["budget", "owner_fault", "native_fts", "cancel", "root"] {
        let inference = ReadGate::default();
        let _release_on_failure = McpFillRelease([inference.clone(), ReadGate::default()]);
        let (fixture, service, fill) = SearchMcpFixture::with_search(
            &[("a.js", &source)],
            |roots| {
                std::sync::Arc::new(
                    trellis::daemon::search::SearchService::with_embedder_for_tests(
                        roots.clone(),
                        std::sync::Arc::new(McpBudgetEmbedderFactory {
                            inference: inference.clone(),
                        }),
                    )
                    .unwrap(),
                )
            },
            true,
        )
        .await;
        // Budget expiry belongs to a query whose head was already resident.
        // Seed documents so the query-only inference gate cannot block fill.
        fixture.seed_vectors(&service);
        fixture.prewarm().await;
        let runtime = fixture
            .registry
            .lock()
            .await
            .runtime(&fixture.identity.root_key)
            .unwrap();
        let scheduler = runtime.browser_scheduler().unwrap();
        // Worker steps publish fill hints even at an unchanged head. Hold future
        // hints, then drain prior work so recovery cannot clear an injected fault.
        // Keep the synchronous guard on a blocking thread, not across an await.
        let worker_gate = ReadGate::default();
        let _release_worker_on_failure = McpFillRelease([worker_gate.clone(), ReadGate::default()]);
        let held = worker_gate.clone();
        let worker = tokio::task::spawn_blocking(move || {
            let _worker = scheduler.hold_worker_for_tests();
            (held.hook())();
        });
        let held = worker_gate.clone();
        tokio::task::spawn_blocking(move || held.await_held(1))
            .await
            .unwrap();
        let waiter = fill.unwrap();
        tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
            .await
            .unwrap();
        let origin = std::time::Instant::now() - trellis::mcp::CALL_DEADLINE;
        runtime.freeze_mcp_call_clock_for_tests(origin);
        let clock = runtime.clone();
        let hooks = trellis::mcp::TransportHooksForTests {
            paused_clock_for_tests: Some(std::sync::Arc::new(move || {
                clock.mcp_call_clock_now_for_tests()
            })),
            ..Default::default()
        };
        let (mut client, mut reader, session) =
            hooked_session(&fixture.identity, &fixture.registry, 211, None, hooks);
        writeln!(
            client,
            "{}",
            call(
                false,
                json!(1),
                "trellis_search_symbols",
                json!({
                    "schemaVersion":1,"query":"orchard","limit":50,
                    "expectedBasis":SearchMcpFixture::basis(fixture.current)
                })
            )
        )
        .unwrap();
        let waiter = inference.clone();
        tokio::task::spawn_blocking(move || waiter.await_held(1))
            .await
            .unwrap_or_else(|error| panic!("{scenario}: inference did not start: {error}"));
        if scenario == "owner_fault" {
            fixture.head().await.set_store_fault_for_tests();
        }
        // Inject only after resident-head inference has actually started.
        if scenario == "native_fts" {
            let store = trellis::testing::retry_admission(|| {
                trellis::store::Store::open_for_tests(fixture._temp.path(), &fixture.identity.root)
            });
            let writer = store.publication_for_tests().unwrap();
            store.corrupt_search_fts_for_tests(&writer).unwrap();
        }
        runtime.advance_mcp_clock_for_tests(Duration::from_secs(3));
        assert_eq!(
            runtime.mcp_call_clock_now_for_tests(),
            origin + Duration::from_secs(3)
        );
        assert!(runtime.mcp_call_clock_now_for_tests() < origin + trellis::mcp::CALL_DEADLINE);
        match scenario {
            "cancel" => {
                writeln!(
                    client,
                    "{}",
                    json!({"jsonrpc":"2.0","method":"notifications/cancelled",
                    "params":{"requestId":1}})
                )
                .unwrap();
                // The following response proves the transport processed the
                // cancellation before inference is allowed to finish.
                writeln!(client, "{}", modern(json!(2), "tools/list", json!({}))).unwrap();
                assert_eq!(next_reply(&mut reader)["id"], 2);
            }
            "root" => {
                fs::rename(
                    &fixture.identity.root,
                    fixture._temp.path().join("moved-checkout"),
                )
                .unwrap();
                fs::create_dir(&fixture.identity.root).unwrap();
            }
            _ => {}
        }
        inference.open();
        if scenario == "cancel" {
            writeln!(client, "{}", modern(json!(3), "tools/list", json!({}))).unwrap();
            assert_eq!(
                next_reply(&mut reader)["id"],
                3,
                "cancelled search emitted evidence"
            );
        } else {
            let reply = next_reply(&mut reader);
            assert_eq!(reply["id"], 1);
            if scenario == "root" {
                tool(&reply, Some("root_changed"), true);
                assert!(reply["result"]["structuredContent"].get("data").is_none());
                schema_pairs.push(search_schema_pair(&reply));
            } else if scenario == "native_fts" || scenario == "owner_fault" {
                assert_eq!(
                    reply["result"]["isError"], true,
                    "{scenario}: required fault must survive optional budget expiry: {reply}"
                );
                tool(&reply, Some("store_unavailable"), true);
                assert!(reply["result"]["structuredContent"].get("data").is_none());
                schema_pairs.push(search_schema_pair(&reply));
            } else {
                tool(&reply, None, true);
                let envelope = &reply["result"]["structuredContent"];
                assert_eq!(
                    envelope["evidenceBasis"],
                    SearchMcpFixture::basis(fixture.current)
                );
                assert_eq!(
                    envelope["data"]["searchIndex"],
                    json!({
                        "state":"unavailable","modelId":null,"modelName":null,"backend":"cpu",
                        "declarations":17,"embedded":0,"lexical":"complete",
                        "unavailableReason":"vector_budget_exceeded"
                    })
                );
                let hits = envelope["data"]["hits"].as_array().unwrap();
                assert_eq!(hits.len(), 17);
                let names: std::collections::BTreeSet<_> = hits
                    .iter()
                    .map(|hit| hit["name"].as_str().unwrap().to_owned())
                    .collect();
                assert_eq!(
                    names,
                    (0..17).map(|index| format!("orchard{index:02}")).collect()
                );
                for (index, hit) in hits.iter().enumerate() {
                    assert_eq!(hit["rank"], index + 1);
                    assert_eq!(hit["matchedBy"], json!(["lexical"]));
                }
                assert_eq!(envelope["partial"], false);
                assert_eq!(envelope["truncated"], false);
                schema_pairs.push(search_schema_pair(&reply));
            }
        }
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
        worker_gate.open();
        worker.await.unwrap();
    }
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_hybrid_ranks_both_arms_and_vector_only_with_partial_then_complete_cache() {
    let factory = std::sync::Arc::new(McpSearchEmbedderFactory::default());
    let (fixture, service, _) = SearchMcpFixture::with_search(
        &[("a.js", "function orchard() {}\nfunction unrelated() {}\n")],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory,
                )
                .unwrap(),
            )
        },
        false,
    )
    .await;
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let model_id = service
        .query_embedding(
            "orchard",
            &cancel,
            std::time::Instant::now() + trellis::testing::HANG_BACKSTOP,
            None,
        )
        .unwrap()
        .model_id()
        .unwrap()
        .to_owned();
    let store = trellis::testing::retry_admission(|| {
        trellis::store::Store::open_for_tests(fixture._temp.path(), &fixture.identity.root)
    });
    let cards = store
        .head_search_cards(Some(fixture.current), None, 16)
        .unwrap()
        .cards;
    assert_eq!(cards.len(), 2);
    drop(store);
    let unrelated = cards
        .iter()
        .find(|card| card.card_text.contains("unrelated"))
        .unwrap();
    let orchard = cards
        .iter()
        .find(|card| card.card_text.contains("orchard"))
        .unwrap();
    service
        .with_vector_writer(|writer| writer.put(&model_id, &unrelated.card_hash, &[1; 1024]))
        .unwrap();
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let mut schema_pairs = Vec::new();
    for (id, complete) in [(1, false), (2, true)] {
        if complete {
            service
                .with_vector_writer(|writer| writer.put(&model_id, &orchard.card_hash, &[1; 1024]))
                .unwrap();
        }
        let reply = search_socket_ask(
            &mut client,
            &mut reader,
            false,
            id,
            json!({"schemaVersion":1,"query":"orchard","limit":10}),
        );
        tool(&reply, None, true);
        let envelope = &reply["result"]["structuredContent"];
        assert_eq!(
            envelope["evidenceBasis"],
            SearchMcpFixture::basis(fixture.current)
        );
        let hits = envelope["data"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
        let expected_names = ["orchard", "unrelated"];
        for (index, (hit, name)) in hits.iter().zip(expected_names).enumerate() {
            assert_eq!(hit["name"], name);
            assert_eq!(hit["rank"], index + 1);
            assert_eq!(hit["evidenceTier"], "syntax");
            assert_eq!(
                emitted_document(&envelope["data"], hit)["document"]["path"],
                "a.js"
            );
            assert!(hit.get("score").is_none());
            let matched = if name == "unrelated" {
                json!(["vector"])
            } else if complete {
                json!(["lexical", "vector"])
            } else {
                json!(["lexical"])
            };
            assert_eq!(hit["matchedBy"], matched);
        }
        let status = &envelope["data"]["searchIndex"];
        assert_eq!(
            status["state"],
            if complete { "complete" } else { "partial" }
        );
        assert_eq!(status["modelId"], model_id);
        assert_eq!(status["backend"], "cpu");
        assert_eq!(status["declarations"], 2);
        assert_eq!(status["embedded"], if complete { 2 } else { 1 });
        assert_eq!(status["lexical"], "complete");
        assert_eq!(status["unavailableReason"], Value::Null);
        assert!(
            status
                .as_object()
                .unwrap()
                .contains_key("unavailableReason")
        );
        assert_eq!(
            envelope["partial"], false,
            "vector misses are not native incompleteness"
        );
        assert_eq!(envelope["truncated"], false);
        assert_eq!(envelope["warnings"][0]["code"], "syntaxOnly");
        schema_pairs.push(search_schema_pair(&reply));
        if !complete {
            let nonexact = search_socket_ask(
                &mut client,
                &mut reader,
                false,
                3,
                json!({"schemaVersion":1,"query":"orchards","limit":10}),
            );
            tool(&nonexact, None, true);
            let data = &nonexact["result"]["structuredContent"]["data"];
            assert_eq!(data["hits"][0]["name"], "unrelated");
            assert_eq!(data["hits"][0]["matchedBy"], json!(["vector"]));
            assert_eq!(data["hits"][1]["name"], "orchard");
            assert_eq!(data["hits"][1]["matchedBy"], json!(["lexical"]));
            schema_pairs.push(search_schema_pair(&nonexact));
        }
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_available_model_keeps_stale_and_corrupt_cache_faults_typed() {
    let mut schema_pairs = Vec::new();
    for stale_format in [true, false] {
        let (fixture, _service, _) = SearchMcpFixture::with_search(
            &[("a.js", "function orchard() {}\n")],
            |roots| {
                std::sync::Arc::new(
                    trellis::daemon::search::SearchService::with_embedder_for_tests(
                        roots.clone(),
                        std::sync::Arc::new(McpSearchEmbedderFactory::default()),
                    )
                    .unwrap(),
                )
            },
            false,
        )
        .await;
        install_unusable_search_cache(&fixture.vectors, stale_format);
        let (mut client, mut reader, session) =
            socket_session(&fixture.identity, &fixture.registry, 211, None);
        let reply = search_socket_ask(
            &mut client,
            &mut reader,
            false,
            1,
            json!({"schemaVersion":1,"query":"orchard"}),
        );
        tool(&reply, Some("store_unavailable"), true);
        let envelope = &reply["result"]["structuredContent"];
        assert!(
            envelope.get("data").is_none(),
            "no partial lexical results on database fault"
        );
        schema_pairs.push(search_schema_pair(&reply));
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
    validate_structured_content(&schema_pairs);
}

struct McpFillRelease([ReadGate; 2]);
impl Drop for McpFillRelease {
    fn drop(&mut self) {
        for gate in &self.0 {
            gate.open();
        }
    }
}

#[derive(Clone)]
struct McpFillEmbedderFactory {
    before_load: Option<ReadGate>,
    first_batch: ReadGate,
    second_batch: ReadGate,
    loads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    batches: std::sync::Arc<std::sync::Mutex<Vec<usize>>>,
    queries: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
impl trellis::search_embed::SearchEmbedderFactory for McpFillEmbedderFactory {
    fn load(
        &self,
        control: &trellis::search_embed::EmbeddingControl<'_>,
    ) -> Result<Box<dyn trellis::search_embed::SearchEmbedder>, trellis::search_embed::EmbedError>
    {
        control.check()?;
        if let Some(gate) = &self.before_load {
            (gate.hook())();
        }
        self.loads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Ok(Box::new(self.clone()))
    }
}
impl trellis::search_embed::SearchEmbedder for McpFillEmbedderFactory {
    fn embed(
        &self,
        texts: &[String],
        kind: trellis::search_embed::InputKind,
        control: &trellis::search_embed::EmbeddingControl<'_>,
    ) -> Result<Vec<trellis::search_embed::QuantizedVector>, trellis::search_embed::EmbedError>
    {
        if kind == trellis::search_embed::InputKind::Document {
            let batch = {
                let mut batches = self.batches.lock().unwrap();
                batches.push(texts.len());
                batches.len()
            };
            match batch {
                1 => (self.first_batch.hook())(),
                2 => (self.second_batch.hook())(),
                _ => panic!("unexpected third document batch"),
            }
        } else {
            self.queries
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        }
        let mut vectors = McpSearchEmbedder.embed(texts, kind, control)?;
        for (text, vector) in texts.iter().zip(&mut vectors) {
            // Give one declaration a distinct direction so vector-only queries
            // prove resident ranking, not just candidate enumeration/counting.
            if (kind == trellis::search_embed::InputKind::Document && text.contains("orchard16"))
                || (kind == trellis::search_embed::InputKind::Query && text == "rainfall")
            {
                vector.values.fill(0);
                vector.values[0] = 127;
            }
        }
        Ok(vectors)
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_fill_serves_queued_query_between_real_batches_and_reports_building() {
    let source: String = (0..17)
        .map(|index| format!("function orchard{index:02}() {{}}\n"))
        .collect();
    let before_load = ReadGate::default();
    let _release_load_on_failure = McpFillRelease([before_load.clone(), ReadGate::default()]);
    let first_batch = ReadGate::default();
    let second_batch = ReadGate::default();
    let _release_on_failure = McpFillRelease([first_batch.clone(), second_batch.clone()]);
    let factory = std::sync::Arc::new(McpFillEmbedderFactory {
        before_load: Some(before_load.clone()),
        first_batch: first_batch.clone(),
        second_batch: second_batch.clone(),
        loads: Default::default(),
        batches: Default::default(),
        queries: Default::default(),
    });
    let (fixture, service, fill) = SearchMcpFixture::with_search(
        &[("a.js", &source)],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory.clone(),
                )
                .unwrap(),
            )
        },
        true,
    )
    .await;
    fixture.startup.open();
    let waiter = before_load.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let cold = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        0,
        json!({"schemaVersion":1,"query":"orchard","limit":50,
            "expectedBasis":SearchMcpFixture::basis(fixture.current)}),
    );
    tool(&cold, None, true);
    let cold_envelope = &cold["result"]["structuredContent"];
    assert_eq!(
        cold_envelope["evidenceBasis"],
        SearchMcpFixture::basis(fixture.current)
    );
    // The owner is building this head (held before its model load), so the
    // lexical-only answer reports building with the measured empty cache.
    assert_eq!(
        cold_envelope["data"]["searchIndex"],
        json!({
            "state":"building","modelId":null,"modelName":null,"backend":"cpu",
            "declarations":17,"embedded":0,"lexical":"complete","unavailableReason":null
        })
    );
    let cold_hits = cold_envelope["data"]["hits"].as_array().unwrap();
    assert_eq!(cold_hits.len(), 17);
    assert!(
        cold_hits
            .iter()
            .all(|hit| hit["matchedBy"] == json!(["lexical"]))
    );
    assert_eq!(cold_envelope["partial"], false);
    assert_eq!(cold_envelope["truncated"], false);
    assert_eq!(
        factory.queries.load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(factory.loads.load(std::sync::atomic::Ordering::Acquire), 0);
    assert!(factory.batches.lock().unwrap().is_empty());
    // The owner has reserved the input candidates before model loading, but
    // has not allocated a resident matrix or its fill scratch yet.
    let candidate_bytes = 17
        * (2 * std::mem::size_of::<trellis::search_rank::SearchCandidate>() + 39 + 64)
        + 2 * std::mem::size_of::<usize>();
    assert_eq!(
        fill.as_ref().unwrap().matrix_storage_bytes(),
        candidate_bytes
    );
    // Receiving the complete reply above while the load gate is still held
    // proves the cold query did not wait or enqueue inference behind the owner.
    before_load.open();
    let waiter = first_batch.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(1),
            "trellis_search_symbols",
            json!({"schemaVersion":1,"query":"orchard","limit":50,
            "expectedBasis":SearchMcpFixture::basis(fixture.current)})
        )
    )
    .unwrap();
    let waiter = service.clone();
    tokio::task::spawn_blocking(move || waiter.wait_for_queued_query_for_tests())
        .await
        .unwrap();
    first_batch.open();
    let waiter = second_batch.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    let building = next_reply(&mut reader);
    assert_eq!(building["id"], 1);
    tool(&building, None, true);
    let envelope = &building["result"]["structuredContent"];
    assert_eq!(
        envelope["evidenceBasis"],
        SearchMcpFixture::basis(fixture.current)
    );
    let status = &envelope["data"]["searchIndex"];
    assert_eq!(status["state"], "building");
    assert!(status["modelId"].is_string());
    assert_eq!(status["backend"], "cpu");
    assert_eq!(status["declarations"], 17);
    // The queued query runs before the asynchronous cache write is published.
    assert_eq!(status["embedded"], 0);
    let hits = envelope["data"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 17);
    assert_eq!(
        hits.iter()
            .filter(|hit| hit["matchedBy"] == json!(["lexical", "vector"]))
            .count(),
        0
    );
    assert_eq!(
        hits.iter()
            .filter(|hit| hit["matchedBy"] == json!(["lexical"]))
            .count(),
        17
    );
    assert_eq!(envelope["partial"], false);
    assert_eq!(
        factory.loads.load(std::sync::atomic::Ordering::Acquire),
        1,
        "query must reuse the loaded fill model rather than loading another job"
    );
    assert_eq!(*factory.batches.lock().unwrap(), [16, 1]);
    second_batch.open();
    let fill = fill.unwrap();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    let complete = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        2,
        json!({"schemaVersion":1,"query":"orchard","limit":50,
            "expectedBasis":SearchMcpFixture::basis(fixture.current)}),
    );
    tool(&complete, None, true);
    let complete_envelope = &complete["result"]["structuredContent"];
    assert_eq!(
        complete_envelope["evidenceBasis"],
        envelope["evidenceBasis"]
    );
    let complete_status = &complete_envelope["data"]["searchIndex"];
    assert_eq!(complete_status["state"], "complete");
    assert_eq!(complete_status["embedded"], 17);
    assert_eq!(complete_status["declarations"], 17);
    assert_eq!(complete_status["modelId"], status["modelId"]);
    assert!(
        complete_envelope["data"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["matchedBy"] == json!(["lexical", "vector"]))
    );
    assert_eq!(complete_envelope["partial"], false);
    let storage_bytes = fill.matrix_storage_bytes();
    assert!(
        storage_bytes > 0,
        "the real fill owner must publish a resident matrix"
    );
    let mut schema_pairs = vec![
        search_schema_pair(&cold),
        search_schema_pair(&building),
        search_schema_pair(&complete),
    ];
    for (id, query, matched) in [
        (3, "orchard", json!(["lexical", "vector"])),
        (4, "rainfall", json!(["vector"])),
    ] {
        let warm = search_socket_ask(
            &mut client,
            &mut reader,
            false,
            id,
            json!({"schemaVersion":1,"query":query,"limit":50,
                "expectedBasis":SearchMcpFixture::basis(fixture.current)}),
        );
        tool(&warm, None, true);
        let envelope = &warm["result"]["structuredContent"];
        assert_eq!(
            envelope["evidenceBasis"],
            complete_envelope["evidenceBasis"]
        );
        assert_eq!(envelope["data"]["searchIndex"], *complete_status);
        let hits = envelope["data"]["hits"].as_array().unwrap();
        if query == "rainfall" {
            assert_eq!(hits[0]["name"], "orchard16");
        }
        let names: std::collections::BTreeSet<_> = hits
            .iter()
            .map(|hit| hit["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            (0..17).map(|index| format!("orchard{index:02}")).collect()
        );
        for (index, hit) in hits.iter().enumerate() {
            assert_eq!(hit["rank"], index + 1);
            assert_eq!(hit["matchedBy"], matched);
        }
        assert_eq!(envelope["partial"], false);
        assert_eq!(envelope["truncated"], false);
        assert_eq!(fill.matrix_storage_bytes(), storage_bytes);
        schema_pairs.push(search_schema_pair(&warm));
    }
    assert_eq!(factory.loads.load(std::sync::atomic::Ordering::Acquire), 1);
    assert_eq!(
        factory.queries.load(std::sync::atomic::Ordering::Acquire),
        4
    );
    assert_eq!(*factory.batches.lock().unwrap(), [16, 1]);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&schema_pairs);
}

/// #777: right after a publication the owner has not yet built the new head's
/// matrix. Search, describe and CLI status report `building` with the measured
/// cache count, never `unavailable` without a reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_reports_building_with_counts_before_owner_builds_new_head() {
    let first_batch = ReadGate::default();
    let second_batch = ReadGate::default();
    first_batch.open();
    second_batch.open();
    let factory = std::sync::Arc::new(McpFillEmbedderFactory {
        before_load: None,
        first_batch,
        second_batch,
        loads: Default::default(),
        batches: Default::default(),
        queries: Default::default(),
    });
    let (fixture, service, fill) = SearchMcpFixture::with_search(
        &[("a.js", "function orchard() {}\nfunction unrelated() {}\n")],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory.clone(),
                )
                .unwrap(),
            )
        },
        true,
    )
    .await;
    // The prior head's two cards already have cached vectors.
    fixture.seed_vectors(&service);
    let fill = fill.unwrap();
    let build = ReadGate::default();
    let _release_on_failure = McpFillRelease([build.clone(), ReadGate::default()]);
    fixture
        .head()
        .await
        .set_before_build_hook_for_tests(build.hook());
    let runtime = fixture
        .registry
        .lock()
        .await
        .runtime(&fixture.identity.root_key)
        .unwrap();
    // Change the checkout while startup reconciliation is held, so releasing
    // it publishes a newer revision. Appending keeps the two existing cards
    // (and their cached vectors) intact.
    fs::write(
        fixture.identity.root.join("a.js"),
        "function orchard() {}\nfunction unrelated() {}\nfunction orchardnew() {}\n",
    )
    .unwrap();
    fixture.startup.open();
    // The owner holds here only after the worker published the new revision
    // and hinted it: it has read the new head but not built its matrix. A
    // hint during reconciliation defers before reading a head, so it cannot
    // reach this hook.
    let waiter = build.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let mut pairs = Vec::new();
    let held = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        1,
        json!({"schemaVersion":1,"query":"orchard","limit":50}),
    );
    tool(&held, None, true);
    let envelope = &held["result"]["structuredContent"];
    let basis = envelope["evidenceBasis"].clone();
    assert_ne!(basis, SearchMcpFixture::basis(fixture.current), "{held}");
    assert_eq!(
        envelope["data"]["searchIndex"],
        json!({
            "state":"building","modelId":null,"modelName":null,"backend":"cpu",
            "declarations":3,"embedded":2,"lexical":"complete","unavailableReason":null
        }),
        "{held}"
    );
    let hits = envelope["data"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["name"], "orchard");
    assert_eq!(hits[0]["matchedBy"], json!(["lexical"]));
    pairs.push(search_schema_pair(&held));
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(2),
            "trellis_workspace_describe",
            json!({"schemaVersion":1})
        )
    )
    .unwrap();
    let describe = next_reply(&mut reader);
    let expected = json!({
        "backend":"cpu","unavailableReason":null,
        "state":"building","declarations":3,"embedded":2
    });
    assert_eq!(
        describe["result"]["structuredContent"]["data"]["searchIndex"], expected,
        "{describe}"
    );
    pairs.push(json!({"name":"trellis_workspace_describe",
        "structuredContent":describe["result"]["structuredContent"]}));
    let status_runtime = runtime.clone();
    let status = tokio::task::spawn_blocking(move || status_runtime.cli_status().unwrap())
        .await
        .unwrap();
    assert_eq!(status["searchIndex"], expected, "{status}");
    build.open();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    assert_eq!(*factory.batches.lock().unwrap(), [1]);
    let complete = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        3,
        json!({"schemaVersion":1,"query":"orchard","limit":50}),
    );
    tool(&complete, None, true);
    let envelope = &complete["result"]["structuredContent"];
    assert_eq!(envelope["evidenceBasis"], basis);
    let index = &envelope["data"]["searchIndex"];
    assert_eq!(index["state"], "complete", "{complete}");
    assert!(index["modelId"].is_string());
    assert_eq!(index["declarations"], 3);
    assert_eq!(index["embedded"], 3);
    assert_eq!(index["unavailableReason"], Value::Null);
    pairs.push(search_schema_pair(&complete));
    // All three surfaces agree once complete: describe and CLI status project
    // the same state and counts that search reports.
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(4),
            "trellis_workspace_describe",
            json!({"schemaVersion":1})
        )
    )
    .unwrap();
    let describe = next_reply(&mut reader);
    let expected = json!({
        "backend":"cpu","unavailableReason":null,
        "state":"complete","declarations":3,"embedded":3
    });
    let described = &describe["result"]["structuredContent"]["data"]["searchIndex"];
    assert_eq!(described, &expected, "{describe}");
    for field in ["state", "declarations", "embedded", "unavailableReason"] {
        assert_eq!(described[field], index[field], "{field}");
    }
    pairs.push(json!({"name":"trellis_workspace_describe",
        "structuredContent":describe["result"]["structuredContent"]}));
    let status = tokio::task::spawn_blocking(move || runtime.cli_status().unwrap())
        .await
        .unwrap();
    assert_eq!(status["searchIndex"], expected);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&pairs);
}

/// A held owner with no matrix for the new head, as #777's building test sets
/// it up: three declarations, two of them cached.
async fn held_building_head() -> (
    SearchMcpFixture,
    std::sync::Arc<trellis::daemon::search::SearchService>,
    std::sync::Arc<trellis::daemon::search_fill::SearchFill>,
    std::sync::Arc<trellis::daemon::registry::CheckoutRuntime>,
    ReadGate,
) {
    let first_batch = ReadGate::default();
    let second_batch = ReadGate::default();
    first_batch.open();
    second_batch.open();
    let factory = std::sync::Arc::new(McpFillEmbedderFactory {
        before_load: None,
        first_batch,
        second_batch,
        loads: Default::default(),
        batches: Default::default(),
        queries: Default::default(),
    });
    let (fixture, service, fill) = SearchMcpFixture::with_search(
        &[("a.js", "function orchard() {}\nfunction unrelated() {}\n")],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory.clone(),
                )
                .unwrap(),
            )
        },
        true,
    )
    .await;
    fixture.seed_vectors(&service);
    let build = ReadGate::default();
    fixture
        .head()
        .await
        .set_before_build_hook_for_tests(build.hook());
    let runtime = fixture
        .registry
        .lock()
        .await
        .runtime(&fixture.identity.root_key)
        .unwrap();
    fs::write(
        fixture.identity.root.join("a.js"),
        "function orchard() {}\nfunction unrelated() {}\nfunction orchardnew() {}\n",
    )
    .unwrap();
    fixture.startup.open();
    let waiter = build.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    (fixture, service, fill.unwrap(), runtime, build)
}

fn describe_search_index(
    client: &mut std::os::unix::net::UnixStream,
    reader: &mut BufReader<std::os::unix::net::UnixStream>,
    id: u64,
) -> (Value, Value) {
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(id),
            "trellis_workspace_describe",
            json!({"schemaVersion":1})
        )
    )
    .unwrap();
    let describe = next_reply(reader);
    let envelope = &describe["result"]["structuredContent"];
    (
        envelope["data"]["searchIndex"].clone(),
        envelope["evidenceBasis"].clone(),
    )
}

/// #806: while the owner has no matrix for the admitted head, describe and CLI
/// status count the head's cached vectors once, not on every call. A cache
/// write is the owner's signal that the count may have changed: the next call
/// measures again and reports the new count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_describe_counts_pending_head_cache_once_per_cache_write() {
    let (fixture, service, fill, runtime, build) = held_building_head().await;
    let _release_on_failure = McpFillRelease([build.clone(), ReadGate::default()]);
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let building = json!({
        "backend":"cpu","unavailableReason":null,
        "state":"building","declarations":3,"embedded":2
    });
    let mut pin = None;
    for id in 1..=3 {
        let (index, basis) = describe_search_index(&mut client, &mut reader, id);
        assert_eq!(index, building, "describe {id}");
        pin.get_or_insert(basis);
    }
    let status_runtime = runtime.clone();
    let status = tokio::task::spawn_blocking(move || status_runtime.cli_status().unwrap())
        .await
        .unwrap();
    assert_eq!(status["searchIndex"], building, "{status}");
    assert_eq!(runtime.search_coverage_scans_for_tests(), 1);
    // Cache the new head's third card while the owner is still held.
    let basis = pin.unwrap();
    let pin = trellis::model::IndexPin {
        index_generation: basis["indexGeneration"].as_str().unwrap().parse().unwrap(),
        index_revision: basis["indexRevision"].as_u64().unwrap(),
    };
    fixture.seed_vectors_at(&service, pin);
    let (index, _) = describe_search_index(&mut client, &mut reader, 4);
    assert_eq!(
        index,
        json!({
            "backend":"cpu","unavailableReason":null,
            "state":"building","declarations":3,"embedded":3
        })
    );
    let (again, _) = describe_search_index(&mut client, &mut reader, 5);
    assert_eq!(again, index);
    assert_eq!(runtime.search_coverage_scans_for_tests(), 2);
    build.open();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    let (index, _) = describe_search_index(&mut client, &mut reader, 6);
    assert_eq!(
        index,
        json!({
            "backend":"cpu","unavailableReason":null,
            "state":"complete","declarations":3,"embedded":3
        })
    );
    assert_eq!(runtime.search_coverage_scans_for_tests(), 2);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #806: the remembered coverage belongs to one admitted head. When the head
/// advances while the owner is still held, describe counts the new head
/// (declarations and cached vectors) instead of reusing the old count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_describe_pending_coverage_follows_an_advanced_head() {
    let (fixture, _service, fill, runtime, build) = held_building_head().await;
    let _release_on_failure = McpFillRelease([build.clone(), ReadGate::default()]);
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let (index, first) = describe_search_index(&mut client, &mut reader, 1);
    assert_eq!(
        index,
        json!({
            "backend":"cpu","unavailableReason":null,
            "state":"building","declarations":3,"embedded":2
        })
    );
    assert_eq!(runtime.search_coverage_scans_for_tests(), 1);
    fs::write(
        fixture.identity.root.join("a.js"),
        "function orchard() {}\nfunction unrelated() {}\nfunction orchardnew() {}\n\
         function orchardlater() {}\n",
    )
    .unwrap();
    let indexing = runtime.clone();
    let root = fixture.identity.root.clone();
    tokio::task::spawn_blocking(move || {
        indexing.cli_index(&trellis::indexer::IndexOptions::new(root))
    })
    .await
    .unwrap()
    .unwrap();
    let advanced = json!({
        "backend":"cpu","unavailableReason":null,
        "state":"building","declarations":4,"embedded":2
    });
    for id in 2..=3 {
        let (index, basis) = describe_search_index(&mut client, &mut reader, id);
        assert!(
            basis["indexRevision"].as_u64().unwrap() > first["indexRevision"].as_u64().unwrap(),
            "{basis}"
        );
        assert_eq!(index, advanced, "describe {id}");
    }
    assert_eq!(runtime.search_coverage_scans_for_tests(), 2);
    // The publishing worker step hints the owner as it ends: wait for that
    // step, so the owner has the newer head queued before it is released.
    let scheduler = runtime.browser_scheduler().unwrap();
    tokio::task::spawn_blocking(move || drop(scheduler.hold_worker_for_tests()))
        .await
        .unwrap();
    build.open();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    let (index, _) = describe_search_index(&mut client, &mut reader, 4);
    assert_eq!(
        index,
        json!({
            "backend":"cpu","unavailableReason":null,
            "state":"complete","declarations":4,"embedded":4
        })
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
}

/// #777 owner ruling (2026-11-07): describe and CLI status never fail on the
/// optional vector cache. With the owner pending and the cache unreadable,
/// both report building with a null count and a typed diagnostic, while
/// search keeps its typed store_unavailable failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_describe_and_status_report_building_without_count_when_cache_unreadable() {
    let first_batch = ReadGate::default();
    let second_batch = ReadGate::default();
    first_batch.open();
    second_batch.open();
    let factory = std::sync::Arc::new(McpFillEmbedderFactory {
        before_load: None,
        first_batch,
        second_batch,
        loads: Default::default(),
        batches: Default::default(),
        queries: Default::default(),
    });
    let (fixture, service, fill) = SearchMcpFixture::with_search(
        &[("a.js", "function orchard() {}\nfunction unrelated() {}\n")],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory.clone(),
                )
                .unwrap(),
            )
        },
        true,
    )
    .await;
    fixture.seed_vectors(&service);
    let fill = fill.unwrap();
    let build = ReadGate::default();
    let _release_on_failure = McpFillRelease([build.clone(), ReadGate::default()]);
    fixture
        .head()
        .await
        .set_before_build_hook_for_tests(build.hook());
    let runtime = fixture
        .registry
        .lock()
        .await
        .runtime(&fixture.identity.root_key)
        .unwrap();
    fs::write(
        fixture.identity.root.join("a.js"),
        "function orchard() {}\nfunction unrelated() {}\nfunction orchardnew() {}\n",
    )
    .unwrap();
    fixture.startup.open();
    let waiter = build.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    // Make the cache path unreadable while the owner is paused. Rename the
    // live file instead of reopening it, so no separate descriptor is closed
    // beneath the owner's SQLite connection.
    let moved = fixture.vectors.with_extension("db.moved");
    fs::rename(&fixture.vectors, &moved).unwrap();
    fs::create_dir(&fixture.vectors).unwrap();
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let mut pairs = Vec::new();
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(1),
            "trellis_workspace_describe",
            json!({"schemaVersion":1})
        )
    )
    .unwrap();
    let describe = next_reply(&mut reader);
    let expected = json!({
        "backend":"cpu","unavailableReason":null,
        "state":"building","declarations":3,"embedded":null,
        "cacheDiagnostic":"vector_cache_unavailable"
    });
    assert_eq!(
        describe["result"]["structuredContent"]["data"]["searchIndex"], expected,
        "{describe}"
    );
    pairs.push(json!({"name":"trellis_workspace_describe",
        "structuredContent":describe["result"]["structuredContent"]}));
    let status_runtime = runtime.clone();
    let status = tokio::task::spawn_blocking(move || status_runtime.cli_status().unwrap())
        .await
        .unwrap();
    assert_eq!(status["searchIndex"], expected, "{status}");
    let failed = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        2,
        json!({"schemaVersion":1,"query":"orchard","limit":50}),
    );
    tool(&failed, Some("store_unavailable"), true);
    // Restore the cache and let the owner finish: nothing was invalidated.
    fs::remove_dir(&fixture.vectors).unwrap();
    fs::rename(&moved, &fixture.vectors).unwrap();
    build.open();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    let status = tokio::task::spawn_blocking(move || runtime.cli_status().unwrap())
        .await
        .unwrap();
    assert_eq!(
        status["searchIndex"],
        json!({
            "backend":"cpu","unavailableReason":null,
            "state":"complete","declarations":3,"embedded":3
        })
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&pairs);
}

/// #781 review finding 1: a memory decline of the old head must not hide a
/// smaller successor the owner admits. Decline a two-card head, publish a
/// one-card successor, pause the owner before its build, and require
/// `building` on search, describe and CLI status.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_smaller_successor_after_memory_decline_reports_building() {
    use trellis::daemon::registry::CheckoutRegistry;
    use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    fs::write(
        root.join("a.js"),
        "function orchard() {}\nfunction unrelated() {}\n",
    )
    .unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root)
        .unwrap()
        .attach_marker()
        .unwrap();
    let topology =
        TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
    let service = std::sync::Arc::new(
        trellis::daemon::search::SearchService::with_embedder_for_tests(
            topology.clone(),
            std::sync::Arc::new(McpSearchEmbedderFactory::default()),
        )
        .unwrap(),
    );
    // A cap that admits a one-card head alone but declines a two-card head.
    let one = trellis::daemon::search_head::full_admission_bytes_for_tests(1);
    let two = trellis::daemon::search_head::full_admission_bytes_for_tests(2);
    assert!(one < two);
    let fill = trellis::daemon::search_fill::SearchFill::start_with_limit_for_tests(
        service.clone(),
        one + (two - one) / 2,
    )
    .unwrap();
    let mut registry = CheckoutRegistry::with_roots(topology);
    registry.set_search_service(service.clone());
    registry.set_search_fill(fill.clone());
    registry.attach_launch(211, &identity).unwrap();
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
    let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
    let scheduler = runtime.browser_scheduler().unwrap();
    let head = scheduler.search_head_slot_for_tests().unwrap();
    // Settle the owner's attempt on the two-card head: it is declined.
    let waiter = head.clone();
    tokio::task::spawn_blocking(move || {
        let budget = trellis::search_embed::QueryBudget::new(
            std::time::Instant::now() + trellis::testing::HANG_BACKSTOP,
            std::sync::Arc::new(std::time::Instant::now),
        );
        waiter.request();
        waiter
            .wait_latest(&std::sync::atomic::AtomicBool::new(false), &budget)
            .unwrap();
    })
    .await
    .unwrap();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    assert_eq!(fill.matrix_storage_bytes(), 0);
    let status_runtime = runtime.clone();
    let declined = tokio::task::spawn_blocking(move || status_runtime.cli_status().unwrap())
        .await
        .unwrap();
    assert_eq!(
        declined["searchIndex"],
        json!({
            "backend":"cpu","unavailableReason":"matrix_not_resident",
            "state":"unavailable","declarations":2,"embedded":0
        }),
        "{declined}"
    );
    let build = ReadGate::default();
    let _release_on_failure = McpFillRelease([build.clone(), ReadGate::default()]);
    head.set_before_build_hook_for_tests(build.hook());
    fs::write(root.join("a.js"), "function orchard() {}\n").unwrap();
    let event = notify::Event::new(notify::EventKind::Modify(notify::event::ModifyKind::Data(
        notify::event::DataChange::Content,
    )))
    .add_path(root.join("a.js"));
    assert!(scheduler.submit_watch_event_for_tests(Ok(event)));
    // The owner admitted the smaller successor and holds before its build.
    let waiter = build.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    let expected = json!({
        "backend":"cpu","unavailableReason":null,
        "state":"building","declarations":1,"embedded":0
    });
    let status_runtime = runtime.clone();
    let status = tokio::task::spawn_blocking(move || status_runtime.cli_status().unwrap())
        .await
        .unwrap();
    assert_eq!(status["searchIndex"], expected, "{status}");
    let (mut client, mut reader, session) = socket_session(&identity, &registry, 211, None);
    let mut pairs = Vec::new();
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(1),
            "trellis_workspace_describe",
            json!({"schemaVersion":1})
        )
    )
    .unwrap();
    let describe = next_reply(&mut reader);
    assert_eq!(
        describe["result"]["structuredContent"]["data"]["searchIndex"], expected,
        "{describe}"
    );
    pairs.push(json!({"name":"trellis_workspace_describe",
        "structuredContent":describe["result"]["structuredContent"]}));
    let reply = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        2,
        json!({"schemaVersion":1,"query":"orchard"}),
    );
    tool(&reply, None, true);
    assert_eq!(
        reply["result"]["structuredContent"]["data"]["searchIndex"],
        json!({
            "state":"building","modelId":null,"modelName":null,"backend":"cpu",
            "declarations":1,"embedded":0,"lexical":"complete","unavailableReason":null
        }),
        "{reply}"
    );
    pairs.push(search_schema_pair(&reply));
    build.open();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    let status = tokio::task::spawn_blocking(move || runtime.cli_status().unwrap())
        .await
        .unwrap();
    assert_eq!(
        status["searchIndex"],
        json!({
            "backend":"cpu","unavailableReason":null,
            "state":"complete","declarations":1,"embedded":1
        })
    );
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&pairs);
}

/// #781 review finding 3: the optional pending-head hash scan honors the
/// 3,000ms vector cutoff. With the owner pending and the frozen MCP clock
/// past the cutoff (before the request deadline), search returns the
/// complete lexical answer with `vector_budget_exceeded` without starting
/// the scan.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_pending_head_count_stops_at_the_vector_cutoff() {
    let factory = std::sync::Arc::new(McpSearchEmbedderFactory::default());
    let (fixture, service, fill) = SearchMcpFixture::with_search(
        &[("a.js", "function orchard() {}\nfunction unrelated() {}\n")],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory.clone(),
                )
                .unwrap(),
            )
        },
        true,
    )
    .await;
    fixture.seed_vectors(&service);
    let fill = fill.unwrap();
    let build = ReadGate::default();
    let admitted = ReadGate::default();
    let _release_on_failure = McpFillRelease([build.clone(), admitted.clone()]);
    fixture
        .head()
        .await
        .set_before_build_hook_for_tests(build.hook());
    let runtime = fixture
        .registry
        .lock()
        .await
        .runtime(&fixture.identity.root_key)
        .unwrap();
    fs::write(
        fixture.identity.root.join("a.js"),
        "function orchard() {}\nfunction unrelated() {}\nfunction orchardnew() {}\n",
    )
    .unwrap();
    fixture.startup.open();
    let waiter = build.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    runtime.set_mcp_admitted_gate_for_tests(admitted.hook());
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(1),
            "trellis_search_symbols",
            json!({"schemaVersion":1,"query":"orchard","limit":50})
        )
    )
    .unwrap();
    let waiter = admitted.clone();
    tokio::task::spawn_blocking(move || waiter.await_held(1))
        .await
        .unwrap();
    // Past the 3,000ms vector cutoff, inside the 5,000ms request deadline.
    runtime.advance_mcp_clock_for_tests(Duration::from_millis(3500));
    let scans = runtime.search_hash_scans_for_tests();
    admitted.open();
    let reply = next_reply(&mut reader);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(
        envelope["data"]["searchIndex"],
        json!({
            "state":"unavailable","modelId":null,"modelName":null,"backend":"cpu",
            "declarations":3,"embedded":0,"lexical":"complete",
            "unavailableReason":"vector_budget_exceeded"
        }),
        "{reply}"
    );
    assert_eq!(envelope["data"]["hits"].as_array().unwrap().len(), 1);
    assert_eq!(envelope["data"]["hits"][0]["matchedBy"], json!(["lexical"]));
    assert_eq!(
        runtime.search_hash_scans_for_tests(),
        scans,
        "optional hash scan ran past the vector cutoff"
    );
    build.open();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&[search_schema_pair(&reply)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_cancelled_or_disconnected_cold_query_does_not_withdraw_owner() {
    let source: String = (0..17)
        .map(|index| format!("function orchard{index:02}() {{}}\n"))
        .collect();
    let mut schema_pairs = Vec::new();
    for disconnect in [false, true] {
        let before_load = ReadGate::default();
        let admitted = ReadGate::default();
        let _release_on_failure = McpFillRelease([before_load.clone(), admitted.clone()]);
        let first_batch = ReadGate::default();
        let second_batch = ReadGate::default();
        first_batch.open();
        second_batch.open();
        let factory = std::sync::Arc::new(McpFillEmbedderFactory {
            before_load: Some(before_load.clone()),
            first_batch,
            second_batch,
            loads: Default::default(),
            batches: Default::default(),
            queries: Default::default(),
        });
        let (fixture, _service, fill) = SearchMcpFixture::with_search(
            &[("a.js", &source)],
            |roots| {
                std::sync::Arc::new(
                    trellis::daemon::search::SearchService::with_embedder_for_tests(
                        roots.clone(),
                        factory.clone(),
                    )
                    .unwrap(),
                )
            },
            true,
        )
        .await;
        fixture.startup.open();
        let waiter = before_load.clone();
        tokio::task::spawn_blocking(move || waiter.await_held(1))
            .await
            .unwrap();
        let runtime = fixture
            .registry
            .lock()
            .await
            .runtime(&fixture.identity.root_key)
            .unwrap();
        runtime.set_mcp_admitted_gate_for_tests(admitted.hook());
        let (finished_tx, finished) = mpsc::sync_channel(1);
        let hooks = trellis::mcp::TransportHooksForTests {
            worker_done: Some(std::sync::Arc::new(move || {
                finished_tx.send(()).unwrap();
            })),
            ..Default::default()
        };
        let (mut client, mut reader, session) =
            hooked_session(&fixture.identity, &fixture.registry, 211, None, hooks);
        writeln!(
            client,
            "{}",
            call(
                false,
                json!(1),
                "trellis_search_symbols",
                json!({"schemaVersion":1,"query":"orchard","limit":50})
            )
        )
        .unwrap();
        let waiter = admitted.clone();
        tokio::task::spawn_blocking(move || waiter.await_held(1))
            .await
            .unwrap();
        assert_eq!(
            factory.queries.load(std::sync::atomic::Ordering::Acquire),
            0
        );
        if !disconnect {
            writeln!(client, "{}", cancel_frame(1)).unwrap();
            writeln!(client, "{}", modern(json!(2), "tools/list", json!({}))).unwrap();
            assert_eq!(next_reply(&mut reader)["id"], 2);
        }
        client.shutdown(std::net::Shutdown::Write).unwrap();
        // EOF handling closes the transport even with the admitted read held.
        // Only then may that worker resume; it cannot race EOF into a reply.
        session.join().unwrap().unwrap();
        admitted.open();
        finished
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        assert!(
            reader.read_line(&mut String::new()).unwrap() == 0,
            "cancelled or disconnected cold query emitted evidence"
        );
        drop(reader);
        drop(client);
        // The transport has finished while the owner is still at its load
        // gate. Only releasing that independent gate permits document fill.
        before_load.open();
        let fill = fill.unwrap();
        let waiter = fill.clone();
        tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
            .await
            .unwrap();
        assert!(fill.matrix_storage_bytes() > 0);
        assert_eq!(
            factory.queries.load(std::sync::atomic::Ordering::Acquire),
            0
        );
        let (mut client, mut reader, session) =
            socket_session(&fixture.identity, &fixture.registry, 211, None);
        let warm = search_socket_ask(
            &mut client,
            &mut reader,
            false,
            3,
            json!({"schemaVersion":1,"query":"rainfall","limit":50}),
        );
        tool(&warm, None, true);
        let envelope = &warm["result"]["structuredContent"];
        assert_eq!(envelope["data"]["searchIndex"]["state"], "complete");
        let hits = envelope["data"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 17);
        assert_eq!(hits[0]["name"], "orchard16");
        assert!(hits.iter().all(|hit| hit["matchedBy"] == json!(["vector"])));
        assert_eq!(
            factory.queries.load(std::sync::atomic::Ordering::Acquire),
            1
        );
        assert_eq!(*factory.batches.lock().unwrap(), [16, 1]);
        schema_pairs.push(search_schema_pair(&warm));
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_residency_lost_before_rank_stays_lexical_at_admitted_pin() {
    let (fixture, service, fill) = SearchMcpFixture::with_search(
        &[("a.js", "function orchard() {}\nfunction unrelated() {}\n")],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    std::sync::Arc::new(McpSearchEmbedderFactory::default()),
                )
                .unwrap(),
            )
        },
        true,
    )
    .await;
    fixture.seed_vectors(&service);
    fixture.prewarm().await;
    let waiter = fill.unwrap();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    let head = fixture.head().await;
    let weak = std::sync::Arc::downgrade(&head);
    let cleared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_cleared = cleared.clone();
    head.set_before_rank_hook_for_tests(std::sync::Arc::new(move || {
        // Query embedding and exact declarations lookup have succeeded. Clear
        // only the matrix, preserving cached vectors to catch stream bypasses.
        weak.upgrade().unwrap().clear_for_tests();
        hook_cleared.store(true, std::sync::atomic::Ordering::Release);
    }));
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let reply = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        1,
        json!({"schemaVersion":1,"query":"orchard","limit":50,
            "expectedBasis":SearchMcpFixture::basis(fixture.current)}),
    );
    tool(&reply, None, true);
    assert!(cleared.load(std::sync::atomic::Ordering::Acquire));
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(
        envelope["evidenceBasis"],
        SearchMcpFixture::basis(fixture.current)
    );
    // Nothing is resident and the owner has no work for this head.
    assert_eq!(
        envelope["data"]["searchIndex"],
        json!({
            "state":"unavailable","modelId":null,"modelName":null,"backend":"cpu",
            "declarations":2,"embedded":0,"lexical":"complete",
            "unavailableReason":"matrix_not_resident"
        })
    );
    let hits = envelope["data"]["hits"].as_array().unwrap();
    assert_eq!(
        hits.len(),
        1,
        "persisted unrelated vectors bypassed residency"
    );
    assert_eq!(hits[0]["name"], "orchard");
    assert_eq!(hits[0]["matchedBy"], json!(["lexical"]));
    assert_eq!(envelope["partial"], false);
    assert_eq!(envelope["truncated"], false);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&[search_schema_pair(&reply)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_oversized_head_stays_lexical_without_query_embedding() {
    let first_batch = ReadGate::default();
    let second_batch = ReadGate::default();
    first_batch.open();
    second_batch.open();
    let factory = std::sync::Arc::new(McpFillEmbedderFactory {
        before_load: None,
        first_batch,
        second_batch,
        loads: Default::default(),
        batches: Default::default(),
        queries: Default::default(),
    });
    let (fixture, _service, fill) = SearchMcpFixture::with_optional_search_limit(
        &[("a.js", "function orchard() {}\nfunction unrelated() {}\n")],
        false,
        |roots| {
            Some(std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory.clone(),
                )
                .unwrap(),
            ))
        },
        true,
        Some(1),
    )
    .await;
    fixture.prewarm().await;
    let fill = fill.unwrap();
    let waiter = fill.clone();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    assert_eq!(fill.matrix_storage_bytes(), 0);
    assert!(factory.batches.lock().unwrap().is_empty());
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let mut schema_pairs = Vec::new();
    for (id, query, count) in [(1, "orchard", 1), (2, "rainfall", 0)] {
        let reply = search_socket_ask(
            &mut client,
            &mut reader,
            false,
            id,
            json!({"schemaVersion":1,"query":query,"limit":50,
                "expectedBasis":SearchMcpFixture::basis(fixture.current)}),
        );
        tool(&reply, None, true);
        let envelope = &reply["result"]["structuredContent"];
        assert_eq!(
            envelope["evidenceBasis"],
            SearchMcpFixture::basis(fixture.current)
        );
        assert_eq!(
            envelope["data"]["searchIndex"],
            json!({
                "state":"unavailable","modelId":null,"modelName":null,"backend":"cpu",
                "declarations":2,"embedded":0,"lexical":"complete",
                "unavailableReason":"matrix_not_resident"
            })
        );
        let hits = envelope["data"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), count);
        assert!(
            hits.iter()
                .all(|hit| hit["matchedBy"] == json!(["lexical"]))
        );
        assert_eq!(envelope["partial"], false);
        assert_eq!(envelope["truncated"], false);
        assert_eq!(
            factory.queries.load(std::sync::atomic::Ordering::Acquire),
            0
        );
        assert_eq!(fill.matrix_storage_bytes(), 0);
        schema_pairs.push(search_schema_pair(&reply));
    }
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_cancelled_or_disconnected_query_does_not_withdraw_real_fill() {
    let source: String = (0..17)
        .map(|index| format!("function orchard{index:02}() {{}}\n"))
        .collect();
    let mut schema_pairs = Vec::new();
    for disconnect in [false, true] {
        let first_batch = ReadGate::default();
        let second_batch = ReadGate::default();
        let _release_on_failure = McpFillRelease([first_batch.clone(), second_batch.clone()]);
        let factory = std::sync::Arc::new(McpFillEmbedderFactory {
            before_load: None,
            first_batch: first_batch.clone(),
            second_batch: second_batch.clone(),
            loads: Default::default(),
            batches: Default::default(),
            queries: Default::default(),
        });
        let (fixture, service, fill) = SearchMcpFixture::with_search(
            &[("a.js", &source)],
            |roots| {
                std::sync::Arc::new(
                    trellis::daemon::search::SearchService::with_embedder_for_tests(
                        roots.clone(),
                        factory.clone(),
                    )
                    .unwrap(),
                )
            },
            true,
        )
        .await;
        fixture.startup.open();
        let waiter = first_batch.clone();
        tokio::task::spawn_blocking(move || waiter.await_held(1))
            .await
            .unwrap();
        let (mut client, mut reader, session) =
            socket_session(&fixture.identity, &fixture.registry, 211, None);
        writeln!(
            client,
            "{}",
            call(
                false,
                json!(1),
                "trellis_search_symbols",
                json!({"schemaVersion":1,"query":"orchard","limit":50,
                    "expectedBasis":SearchMcpFixture::basis(fixture.current)})
            )
        )
        .unwrap();
        let waiter = service.clone();
        tokio::task::spawn_blocking(move || waiter.wait_for_queued_query_for_tests())
            .await
            .unwrap();
        if !disconnect {
            writeln!(client, "{}", cancel_frame(1)).unwrap();
            writeln!(client, "{}", modern(json!(2), "tools/list", json!({}))).unwrap();
            assert_eq!(next_reply(&mut reader)["id"], 2);
        }
        drop(reader);
        drop(client);
        // Transport completion proves cancellation/EOF has been handled while
        // document inference is still blocked. The checkout's owner must survive it.
        session.join().unwrap().unwrap();
        first_batch.open();
        let waiter = second_batch.clone();
        tokio::task::spawn_blocking(move || waiter.await_held(1))
            .await
            .unwrap();
        second_batch.open();
        let fill = fill.unwrap();
        let waiter = fill.clone();
        tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
            .await
            .unwrap();
        assert!(fill.matrix_storage_bytes() > 0);
        let (mut client, mut reader, session) =
            socket_session(&fixture.identity, &fixture.registry, 211, None);
        let reply = search_socket_ask(
            &mut client,
            &mut reader,
            false,
            3,
            json!({"schemaVersion":1,"query":"rainfall","limit":50,
                "expectedBasis":SearchMcpFixture::basis(fixture.current)}),
        );
        tool(&reply, None, true);
        let envelope = &reply["result"]["structuredContent"];
        assert_eq!(
            envelope["evidenceBasis"],
            SearchMcpFixture::basis(fixture.current)
        );
        let status = &envelope["data"]["searchIndex"];
        assert_eq!(status["state"], "complete");
        assert_eq!(status["declarations"], 17);
        assert_eq!(status["embedded"], 17);
        let hits = envelope["data"]["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 17);
        assert_eq!(hits[0]["name"], "orchard16");
        let names: std::collections::BTreeSet<_> = hits
            .iter()
            .map(|hit| hit["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            (0..17).map(|index| format!("orchard{index:02}")).collect()
        );
        for (index, hit) in hits.iter().enumerate() {
            assert_eq!(hit["rank"], index + 1);
            assert_eq!(hit["matchedBy"], json!(["vector"]));
        }
        assert_eq!(envelope["partial"], false);
        assert_eq!(factory.loads.load(std::sync::atomic::Ordering::Acquire), 1);
        assert_eq!(
            factory.queries.load(std::sync::atomic::Ordering::Acquire),
            1
        );
        assert_eq!(*factory.batches.lock().unwrap(), [16, 1]);
        schema_pairs.push(search_schema_pair(&reply));
        drop(reader);
        drop(client);
        session.join().unwrap().unwrap();
    }
    validate_structured_content(&schema_pairs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_background_store_fault_survives_cold_unavailable_fallback() {
    let factory = std::sync::Arc::new(McpSearchEmbedderFactory::default());
    let (fixture, _service, fill) = SearchMcpFixture::with_search(
        &[("a.js", "function orchard() {}\n")],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory.clone(),
                )
                .unwrap(),
            )
        },
        true,
    )
    .await;
    // No cache connection exists. A directory at the database path makes each
    // owner prepare fail, without corrupting a live SQLite file.
    fs::create_dir_all(&fixture.vectors).unwrap();
    let head = fixture.head().await;
    head.request();
    let waiter = fill.unwrap();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    assert!(
        head.fault()
            .unwrap_err()
            .is::<trellis::search_vectors::StoreUnavailable>()
    );
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    let reply = search_socket_ask(
        &mut client,
        &mut reader,
        false,
        1,
        json!({"schemaVersion":1,"query":"orchard",
            "expectedBasis":SearchMcpFixture::basis(fixture.current)}),
    );
    tool(&reply, Some("store_unavailable"), true);
    assert!(
        reply["result"]["structuredContent"].get("data").is_none(),
        "a known owner fault cannot become successful lexical-only evidence"
    );
    assert_eq!(factory.loads.load(std::sync::atomic::Ordering::Acquire), 0);
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&[search_schema_pair(&reply)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_search_during_startup_defers_background_head_and_serves_valid_prior_head() {
    let factory = std::sync::Arc::new(McpSearchEmbedderFactory::default());
    let (fixture, _service, fill) = SearchMcpFixture::with_search(
        &[("a.js", "function orchard() {}\n")],
        |roots| {
            std::sync::Arc::new(
                trellis::daemon::search::SearchService::with_embedder_for_tests(
                    roots.clone(),
                    factory.clone(),
                )
                .unwrap(),
            )
        },
        true,
    )
    .await;
    let runtime = fixture
        .registry
        .lock()
        .await
        .runtime(&fixture.identity.root_key)
        .unwrap();
    let (mut client, mut reader, session) =
        socket_session(&fixture.identity, &fixture.registry, 211, None);
    writeln!(
        client,
        "{}",
        call(
            false,
            json!(1),
            "trellis_search_symbols",
            json!({"schemaVersion":1,"query":"orchard",
                "expectedBasis":SearchMcpFixture::basis(fixture.current)})
        )
    )
    .unwrap();
    // The cold query must finish while H remains held. It cannot wait for
    // background preparation or create a query embedding job.
    let reply = next_reply(&mut reader);
    // Owner idle proves its request was independently deferred during H.
    let waiter = fill.unwrap();
    tokio::task::spawn_blocking(move || waiter.wait_until_idle_for_tests())
        .await
        .unwrap();
    let head = runtime
        .browser_scheduler()
        .unwrap()
        .search_head_slot_for_tests()
        .unwrap();
    head.fault().unwrap();
    assert_eq!(reply["id"], 1);
    tool(&reply, None, true);
    let envelope = &reply["result"]["structuredContent"];
    assert_eq!(
        envelope["evidenceBasis"],
        SearchMcpFixture::basis(fixture.current)
    );
    assert_eq!(envelope["catchingUp"], true);
    let hits = envelope["data"]["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["name"], "orchard");
    assert_eq!(hits[0]["matchedBy"], json!(["lexical"]));
    assert_eq!(envelope["partial"], false);
    assert_eq!(envelope["truncated"], false);
    assert_eq!(envelope["data"]["searchIndex"]["declarations"], 1);
    // The owner deferred this head until reconciliation finishes; that work
    // is still pending, so lexical-only search reports building (#777).
    assert_eq!(
        envelope["data"]["searchIndex"],
        json!({
            "state":"building","modelId":null,"modelName":null,"backend":"cpu",
            "declarations":1,"embedded":0,"lexical":"complete","unavailableReason":null
        })
    );
    assert_eq!(factory.loads.load(std::sync::atomic::Ordering::Acquire), 0);
    // Do not release startup until the prior-head success and catchingUp flag
    // have been asserted. The fixture releases H only during teardown.
    drop(reader);
    drop(client);
    session.join().unwrap().unwrap();
    validate_structured_content(&[search_schema_pair(&reply)]);
}

use crate::graph_fixtures;

/// A published synthetic index served through an in-process daemon-socket MCP session. Startup
/// reconciliation is held for the session's life, so the published head is served as published.
struct GraphMcp {
    temp: tempfile::TempDir,
    root: PathBuf,
    pins: Vec<trellis::model::IndexPin>,
    names: std::collections::BTreeMap<String, String>,
    revision_names: Vec<std::collections::BTreeMap<String, String>>,
    runtime: std::sync::Arc<trellis::daemon::registry::CheckoutRuntime>,
    client: std::os::unix::net::UnixStream,
    reader: BufReader<std::os::unix::net::UnixStream>,
    session: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    release: mpsc::SyncSender<()>,
    _entered: Receiver<()>,
    next_id: i64,
    pairs: Vec<Value>,
}

impl GraphMcp {
    async fn start(
        session: u64,
        specs: &[graph_fixtures::Spec],
        hooks: trellis::mcp::TransportHooksForTests,
    ) -> Self {
        use trellis::daemon::registry::CheckoutRegistry;
        use trellis::store::topology::{TopologyRoots, WorkspaceIdentity};
        let temp = tempfile::tempdir().unwrap();
        let root = checkout(temp.path());
        let identity = WorkspaceIdentity::discover(Some(&root), &root)
            .unwrap()
            .attach_marker()
            .unwrap();
        let mut publisher = graph_fixtures::Publisher::open(temp.path(), &root);
        publisher
            .store
            .mcp_prepare_virgin_queue_for_tests()
            .unwrap();
        let mut revision_names = Vec::new();
        let pins = specs
            .iter()
            .map(|spec| {
                let pin = publisher.publish(spec);
                revision_names.push(
                    publisher
                        .native()
                        .declarations
                        .iter()
                        .filter_map(|d| Some((d.syntax_id.clone(), d.name.clone()?)))
                        .collect(),
                );
                pin
            })
            .collect();
        let names = publisher
            .native()
            .declarations
            .iter()
            .filter_map(|d| Some((d.syntax_id.clone(), d.name.clone()?)))
            .collect();
        drop(publisher);
        let topology =
            TopologyRoots::isolated_for_tests(temp.path().join("cache"), temp.path().join("data"));
        let mut registry = CheckoutRegistry::with_roots(topology);
        registry.attach_launch(session, &identity).unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release, release_rx) = mpsc::sync_channel(1);
        let release_rx = std::sync::Mutex::new(release_rx);
        registry.set_next_startup_hook_for_tests(std::sync::Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap();
        }));
        let registry = std::sync::Arc::new(tokio::sync::Mutex::new(registry));
        let (mut client, mut reader, thread) =
            hooked_session(&identity, &registry, session, None, hooks);
        writeln!(client, "{}", describe_call(1)).unwrap();
        entered_rx
            .recv_timeout(trellis::testing::HANG_BACKSTOP)
            .unwrap();
        tool(&next_reply(&mut reader), None, true);
        let runtime = registry
            .lock()
            .await
            .runtime(&identity.root_key)
            .expect("the served checkout has a runtime");
        Self {
            temp,
            root,
            pins,
            names,
            revision_names,
            runtime,
            client,
            reader,
            session: Some(thread),
            release,
            _entered: entered_rx,
            next_id: 1,
            pairs: vec![],
        }
    }

    fn id_of(&self, name: &str) -> String {
        let mut found = self.names.iter().filter(|(_, n)| *n == name);
        let (id, _) = found
            .next()
            .unwrap_or_else(|| panic!("no declaration {name}"));
        id.clone()
    }

    /// Tool arguments for the root declaration `name`; `extra` fields are merged in.
    fn args(&self, name: &str, extra: Value) -> Value {
        let mut arguments = json!({"schemaVersion":1,"symbolId":self.id_of(name)});
        for (key, value) in extra.as_object().unwrap() {
            arguments[key] = value.clone();
        }
        arguments
    }

    fn send(&mut self, arguments: Value) -> i64 {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_call_graph","arguments":arguments})
            )
        )
        .unwrap();
        id
    }

    fn record(&mut self, reply: &Value) -> Value {
        let envelope = reply["result"]["structuredContent"].clone();
        self.pairs
            .push(json!({"name":"trellis_call_graph","structuredContent":envelope}));
        envelope
    }

    /// A successful call of any tool; validated against the catalog schema in `finish`.
    fn call_tool(&mut self, name: &str, arguments: Value) -> Value {
        self.next_id += 1;
        let id = self.next_id;
        writeln!(
            self.client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":name,"arguments":arguments})
            )
        )
        .unwrap();
        let reply = next_reply(&mut self.reader);
        assert_eq!(reply["id"], id);
        tool(&reply, None, true);
        let envelope = reply["result"]["structuredContent"].clone();
        self.pairs
            .push(json!({"name":name,"structuredContent":envelope}));
        envelope
    }

    /// A successful call; the envelope is validated against the catalog schema in `finish`.
    fn graph(&mut self, arguments: Value) -> Value {
        let id = self.send(arguments);
        let reply = next_reply(&mut self.reader);
        assert_eq!(reply["id"], id);
        tool(&reply, None, true);
        self.record(&reply)
    }

    fn failure(&mut self, arguments: Value, code: &str) -> Value {
        let id = self.send(arguments);
        let reply = next_reply(&mut self.reader);
        assert_eq!(reply["id"], id);
        tool(&reply, Some(code), true);
        self.record(&reply)
    }

    /// The store answer, optionally projected to a frozen edge prefix, in its full wire
    /// shape: each proof's `basis` becomes its `basisRef` into the canonical basis table.
    fn store_answer(&self, arguments: &Value, prefix: Option<usize>) -> Value {
        let query = trellis::mcp::tools::call_graph_query(arguments);
        let store = trellis::testing::retry_admission(|| {
            trellis::store::Store::open_for_tests(self.temp.path(), &self.root)
        });
        let answer = store
            .evidence_response()
            .unwrap()
            .graph_query(&query, None, &mut || Ok(()))
            .unwrap();
        let mut result = serde_json::to_value(match prefix {
            Some(k) => answer.prefix(k).unwrap(),
            None => answer.outcome.unwrap(),
        })
        .unwrap();
        let bases: std::collections::BTreeSet<String> = result["provenance"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|proof| !proof["basis"].is_null())
            .map(|proof| serde_json::to_string(&proof["basis"]).unwrap())
            .collect();
        for proof in result["provenance"].as_array_mut().unwrap() {
            let proof = proof.as_object_mut().unwrap();
            let basis = proof.remove("basis").unwrap();
            let reference = if basis.is_null() {
                Value::Null
            } else {
                let key = serde_json::to_string(&basis).unwrap();
                json!(bases.iter().position(|b| *b == key).unwrap())
            };
            proof.insert("basisRef".into(), reference);
        }
        result
    }

    fn name(&self, id: &Value) -> String {
        id.as_str()
            .map_or("-".to_owned(), |id| self.names[id].clone())
    }

    /// `(nodes, edges, frontier)` in the contract's compact notation, by declaration name.
    fn shape(&self, result: &Value) -> (Vec<String>, Vec<String>, Vec<String>) {
        let dash = |value: &Value| {
            if value.is_null() {
                "-".to_owned()
            } else {
                value.to_string().trim_matches('"').to_owned()
            }
        };
        let nodes = result["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| {
                format!(
                    "{}:{}",
                    self.name(&n["declaration"]["syntaxId"]),
                    n["depth"]
                )
            })
            .collect();
        let mut spelling = std::collections::BTreeMap::new();
        let edges = result["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                spelling.insert(
                    e["call"]["id"].clone().to_string(),
                    e["call"]["spelling"].clone(),
                );
                format!(
                    "{}:{}>{}:{}:{}",
                    dash(&e["call"]["spelling"]),
                    self.name(&e["from"]),
                    self.name(&e["to"]),
                    dash(&e["visit"]),
                    dash(&e["boundaryReason"])
                )
            })
            .collect();
        let frontier = result["frontier"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                let call = spelling
                    .get(&f["callId"].to_string())
                    .map_or("-".to_owned(), dash);
                format!(
                    "{}:{}:{}:{}:{}:{}",
                    dash(&f["reason"]),
                    self.name(&f["nodeId"]),
                    call,
                    self.name(&f["targetId"]),
                    dash(&f["nextOrdinal"]),
                    dash(&f["omittedCalls"])
                )
            })
            .collect();
        (nodes, edges, frontier)
    }

    /// One graph case: the hand-evaluated arrays, and `data.result` equal to the store reader's.
    fn case(
        &mut self,
        root: &str,
        extra: Value,
        nodes: &[&str],
        edges: &[&str],
        frontier: &[&str],
        (truncated, partial): (bool, bool),
    ) -> Value {
        let arguments = self.args(root, extra);
        let envelope = self.graph(arguments.clone());
        let result = &envelope["data"]["result"];
        let (got_nodes, got_edges, got_frontier) = self.shape(result);
        assert_eq!(got_nodes, nodes, "{root} nodes");
        assert_eq!(got_edges, edges, "{root} edges");
        assert_eq!(got_frontier, frontier, "{root} frontier");
        assert_eq!(result["truncated"], truncated, "{root} truncated");
        assert_eq!(result["partial"], partial, "{root} partial");
        assert_eq!(envelope["truncated"], truncated);
        assert_eq!(envelope["partial"], partial);
        assert_eq!(envelope["warnings"], result["warnings"]);
        assert_eq!(
            envelope["truncationReason"],
            if truncated {
                json!("limit")
            } else {
                Value::Null
            }
        );
        assert_eq!(*result, self.store_answer(&arguments, None), "{root}");
        envelope
    }

    fn warning_keys(envelope: &Value) -> Vec<(String, Option<String>)> {
        envelope["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["code"].as_str().unwrap().to_owned(),
                    w["provenanceId"].as_str().map(str::to_owned),
                )
            })
            .collect()
    }

    fn finish(mut self) {
        validate_structured_content(&self.pairs);
        let _ = self.release.send(());
        let session = self.session.take().unwrap();
        drop(self.reader);
        drop(self.client);
        session.join().unwrap().unwrap();
    }
}

fn keys(items: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
    items
        .iter()
        .map(|(code, id)| (code.to_string(), id.map(str::to_string)))
        .collect()
}

fn with_bindings(
    files: &[(&str, &str)],
    bindings: Vec<graph_fixtures::Bind>,
) -> graph_fixtures::Spec {
    let mut spec = graph_fixtures::Spec::new(files);
    spec.bindings = bindings;
    spec
}

/// #528 review: compact changes presentation, never the optional evidence chosen for the
/// same required prefix. Six bound calls, each with eight long-path native hints: full and
/// compact show the same hints and documents for the same six calls. Both requests name
/// the same `includeExternal` policy (#665): the modes' different defaults change the
/// envelope (`externalOmitted`) by a few bytes near the hint page target, which this test
/// does not compare; `compact_outgoing_leaves_external_calls_out_unless_asked` covers them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_outgoing_keeps_the_full_answers_hints_for_the_same_calls() {
    let mut files = vec![(
        "src/lib.rs".to_owned(),
        "fn run() { foo(); foo(); foo(); foo(); foo(); foo(); }\n".to_owned(),
    )];
    for i in 0..8 {
        files.push((
            format!(
                "src/candidate{i}/{}/a.rs",
                vec!["x".repeat(70); 5].join("/")
            ),
            "pub fn foo() {}\n".to_owned(),
        ));
    }
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    let spec = with_bindings(
        &files,
        (0..6)
            .map(|i| graph_fixtures::bind("run", i, "foo"))
            .collect(),
    );
    let mut mcp = GraphMcp::start(528, &[spec], Default::default()).await;
    let full = mcp.call_tool(
        "trellis_outgoing_calls",
        mcp.args("run", json!({"limit":6,"includeExternal":false})),
    );
    let compact = mcp.call_tool(
        "trellis_outgoing_calls",
        mcp.args(
            "run",
            json!({"limit":6,"compact":true,"includeExternal":false}),
        ),
    );
    let calls = |v: &Value| v["data"]["calls"].as_array().unwrap().clone();
    assert_eq!(calls(&full).len(), 6, "{full}");
    assert_eq!(calls(&compact).len(), 6, "{compact}");
    let hints = |v: &Value| {
        calls(v)
            .iter()
            .map(|c| c["nativeCandidates"].clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(hints(&full), hints(&compact));
    assert_eq!(full["data"]["documents"], compact["data"]["documents"]);
    assert_eq!(full["warnings"], compact["warnings"]);
    assert_eq!(full["partial"], compact["partial"]);
    // The page target bounds the optional hints of the full answer.
    assert!(
        hints(&full)
            .iter()
            .any(|h| h["omitted"].as_u64().unwrap_or(0) > 0),
        "{full}"
    );
    mcp.finish();
}

/// #665: compact outgoing calls leave calls bound to external symbols (`Ok`, `now`, ...) out
/// before the limit and count them; `includeExternal` keeps them, and the full answer keeps
/// them unless asked not to.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_outgoing_leaves_external_calls_out_unless_asked() {
    use graph_fixtures::{Bind, bind};
    use trellis::semantic_evidence::Resolution;
    let external = |nth| Bind {
        resolution: Resolution::External,
        target: None,
        ..bind("run", nth, "-")
    };
    let spec = with_bindings(
        &[(
            "src/lib.rs",
            "fn run() { ok(); helper(); now(); leaf(); err(); }\nfn helper() {}\nfn leaf() {}\n",
        )],
        vec![
            external(0),
            bind("run", 1, "helper"),
            external(2),
            bind("run", 3, "leaf"),
            external(4),
        ],
    );
    let mut mcp = GraphMcp::start(665, &[spec], Default::default()).await;
    let callees = |reply: &Value| -> Vec<String> {
        reply["data"]["calls"]
            .as_array()
            .unwrap()
            .iter()
            .map(|call| call["calleeText"].as_str().unwrap().to_owned())
            .collect()
    };
    let compact = mcp.call_tool(
        "trellis_outgoing_calls",
        mcp.args("run", json!({"compact":true})),
    );
    assert_eq!(callees(&compact), ["helper", "leaf"], "{compact}");
    assert_eq!(compact["data"]["externalOmitted"], 3);
    assert_eq!(compact["truncated"], false);
    // The limit counts kept calls only.
    let one = mcp.call_tool(
        "trellis_outgoing_calls",
        mcp.args("run", json!({"compact":true,"limit":1})),
    );
    assert_eq!(callees(&one), ["helper"]);
    assert_eq!(one["truncationReason"], "limit");
    for (extra, expected, omitted) in [
        (
            json!({"compact":true,"includeExternal":true}),
            &["ok", "helper", "now", "leaf", "err"][..],
            None,
        ),
        (json!({}), &["ok", "helper", "now", "leaf", "err"][..], None),
        (
            json!({"includeExternal":false}),
            &["helper", "leaf"][..],
            Some(3),
        ),
    ] {
        let reply = mcp.call_tool("trellis_outgoing_calls", mcp.args("run", extra.clone()));
        assert_eq!(callees(&reply), expected, "{extra}");
        assert_eq!(
            reply["data"].get("externalOmitted").and_then(Value::as_u64),
            omitted,
            "{extra}"
        );
    }
    mcp.finish();
}

/// #665: `compact` reaches the call graph through MCP, with its own default limits, and
/// the full answer emits each semantic basis once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_graph_compact_and_basis_table_through_mcp() {
    use graph_fixtures::bind;
    let spec = with_bindings(
        &[(
            "a.rs",
            "fn a() { b(); c(); }\nfn b() { d(); }\nfn c() {}\nfn d() {}\n",
        )],
        vec![bind("a", 0, "b"), bind("a", 1, "c"), bind("b", 0, "d")],
    );
    let mut mcp = GraphMcp::start(666, &[spec], Default::default()).await;
    let producer = json!({"semanticProducerId":graph_fixtures::PRODUCER});
    let full = mcp.graph(mcp.args("a", producer.clone()));
    let mut compact_args = producer.clone();
    compact_args["compact"] = json!(true);
    let compact = mcp.graph(mcp.args("a", compact_args));
    assert_eq!(full["data"]["requested"]["maxNodes"], 8);
    assert_eq!(full["data"]["requested"]["maxCalls"], 10);
    assert_eq!(compact["data"]["requested"]["maxNodes"], 50);
    assert_eq!(compact["data"]["requested"]["maxCalls"], 60);
    let bases = full["data"]["semanticBases"].as_array().unwrap();
    assert_eq!(bases.len(), 1, "{full}");
    let proofs = full["data"]["result"]["provenance"].as_array().unwrap();
    assert!(proofs.iter().all(|p| p.get("basis").is_none()));
    assert!(proofs.iter().any(|p| p["basisRef"] == 0));
    assert_eq!(
        full["data"]["result"],
        mcp.store_answer(&mcp.args("a", producer.clone()), None)
    );
    let (whole, small) = (&full["data"]["result"], &compact["data"]["result"]);
    assert_eq!(mcp.shape(whole), mcp.shape(small));
    assert!(small.get("provenance").is_none() && small.get("coverage").is_none());
    assert!(compact["data"].get("semanticBases").is_none());
    // #715: every compact node keeps its stable lookup key; the full declaration is the
    // contract record and has none.
    let nodes = small["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 4, "{compact}");
    for node in nodes {
        let declaration = &node["declaration"];
        let name = declaration["name"].as_str().unwrap();
        assert_eq!(declaration["stableLookupKey"], format!("a.rs::{name}"));
    }
    assert!(
        whole["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|node| node["declaration"].get("stableLookupKey").is_none())
    );
    for edge in small["edges"].as_array().unwrap() {
        assert!(edge.get("binding").is_none());
        assert_eq!(edge["resolution"], "resolved");
    }
    for envelope in [&full, &compact] {
        assert_eq!(
            envelope["data"]["responseOmitted"],
            json!({"nodes":0,"edges":0})
        );
    }
    mcp.finish();
}

/// #778: without semantic evidence every call is a boundary. Calls whose callee name
/// names a declaration in the admitted snapshot (the Decision 0012 name relation) rank
/// before the other name-only calls, so constructors and standard-library methods no
/// longer crowd the project's calls out of `maxCalls`; the cut stays exactly counted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_graph_ranks_project_named_calls_first_through_mcp() {
    let spec = graph_fixtures::Spec::new(&[(
        "a.rs",
        "fn run(x: R) -> R {\n    Some(1);\n    x.clone();\n    x.map_err(e);\n    Ok(2);\n    helper();\n    x.into_response();\n    other();\n    Err(3)\n}\nfn helper() {}\nfn other() {}\n",
    )]);
    let mut mcp = GraphMcp::start(778, &[spec], Default::default()).await;
    let spellings = |envelope: &Value| -> Vec<String> {
        envelope["data"]["result"]["edges"]
            .as_array()
            .unwrap()
            .iter()
            .map(|edge| edge["call"]["spelling"].as_str().unwrap().to_owned())
            .collect()
    };
    let all = mcp.graph(mcp.args("run", json!({"compact":true})));
    assert_eq!(
        spellings(&all),
        [
            "helper",
            "other",
            "Some",
            "clone",
            "map_err",
            "Ok",
            "into_response",
            "Err"
        ],
        "{all}"
    );
    assert_eq!(all["truncated"], false);
    for compact in [true, false] {
        let arguments = mcp.args("run", json!({"compact":compact,"maxCalls":3}));
        let cut = mcp.graph(arguments.clone());
        assert_eq!(spellings(&cut), ["helper", "other", "Some"], "{cut}");
        let result = &cut["data"]["result"];
        assert_eq!(result["omitted"]["callLimit"], 5, "{cut}");
        assert_eq!(
            result["frontier"],
            json!([{"reason":"callLimit","nodeId":mcp.id_of("run"),"callId":null,
                "targetId":null,"nextOrdinal":null,"omittedCalls":5}]),
            "{cut}"
        );
        assert_eq!(cut["truncationReason"], "limit");
        if !compact {
            assert_eq!(*result, mcp.store_answer(&arguments, None));
        }
    }
    mcp.finish();
}

/// #778 (owner ruling 2026-11-07): semantic bindings take precedence. The name relation
/// orders only calls with no selected binding; a bound boundary keeps the third rank even
/// when its name matches no declaration, so it is never ranked below an unbound call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_graph_name_order_applies_only_to_unbound_calls_through_mcp() {
    use graph_fixtures::{Bind, bind};
    use trellis::semantic_evidence::Resolution;
    let spec = with_bindings(
        &[(
            "a.rs",
            "fn run(x: R) {\n    Some(1);\n    x.mystery();\n    helper();\n}\nfn helper() {}\n",
        )],
        vec![Bind {
            resolution: Resolution::Unresolved,
            target: None,
            ..bind("run", 1, "-")
        }],
    );
    let mut mcp = GraphMcp::start(7781, &[spec], Default::default()).await;
    let producer = json!({"semanticProducerId":graph_fixtures::PRODUCER,"compact":true});
    let reply = mcp.graph(mcp.args("run", producer));
    let edges = reply["data"]["result"]["edges"].as_array().unwrap();
    let order: Vec<(&str, &str)> = edges
        .iter()
        .map(|edge| {
            (
                edge["call"]["spelling"].as_str().unwrap(),
                edge["boundaryReason"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        order,
        [
            ("mystery", "unresolved"),
            ("helper", "missingEvidence"),
            ("Some", "missingEvidence")
        ],
        "{reply}"
    );
    mcp.finish();
}

/// #715: `compact` reaches find (query and lookup key) and search through MCP. Each item
/// keeps its stable lookup key and path, the answer drops documents, coverage and
/// provenance, and full and compact `scopeCoverage` list only the result languages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compact_find_and_search_through_mcp() {
    let spec = graph_fixtures::Spec::new(&[
        (
            "src/lib.rs",
            "fn navigation_at() {}\nfn navigation_in() {}\n",
        ),
        ("web/app.js", "function navigationAt() {}\n"),
    ]);
    let mut mcp = GraphMcp::start(715, &[spec], Default::default()).await;
    let languages = |envelope: &Value| -> Vec<String> {
        envelope["data"]["scopeCoverage"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["language"].as_str().unwrap().to_owned())
            .collect()
    };
    for (name, items, arguments) in [
        (
            "trellis_find_symbols",
            "symbols",
            json!({"schemaVersion":1,"query":"navigation_"}),
        ),
        (
            "trellis_find_symbols",
            "symbols",
            json!({"schemaVersion":1,"lookupKey":"src/lib.rs::navigation_at"}),
        ),
        (
            "trellis_search_symbols",
            "hits",
            json!({"schemaVersion":1,"query":"navigation"}),
        ),
    ] {
        let full = mcp.call_tool(name, arguments.clone());
        let mut compact_args = arguments;
        compact_args["compact"] = json!(true);
        let compact = mcp.call_tool(name, compact_args);
        let shown = compact["data"][items].as_array().unwrap();
        assert!(!shown.is_empty(), "{full}");
        assert_eq!(shown.len(), full["data"][items].as_array().unwrap().len());
        for item in shown {
            let key = item["stableLookupKey"].as_str().unwrap();
            let path = item["path"].as_str().unwrap();
            assert_eq!(key, format!("{path}::{}", item["name"].as_str().unwrap()));
            assert!(item.get("doc").is_none() && item.get("range").is_none());
        }
        for field in ["documents", "coverage", "provenance", "coverageDiagnostics"] {
            assert!(compact["data"].get(field).is_none(), "{field}");
            assert!(full["data"].get(field).is_some(), "{field}");
        }
        assert_eq!(
            compact["data"]["scopeCoverage"],
            full["data"]["scopeCoverage"]
        );
        let mut expected: Vec<String> = shown
            .iter()
            .map(|item| {
                if item["path"].as_str().unwrap().ends_with(".rs") {
                    "rust".to_owned()
                } else {
                    "javascript".to_owned()
                }
            })
            .collect();
        expected.sort_by_key(|language| language != "rust");
        expected.dedup();
        assert_eq!(languages(&full), expected, "{full}");
        for field in ["warnings", "partial", "truncated", "truncationReason"] {
            assert_eq!(compact[field], full[field], "{name} {field}");
        }
    }
    mcp.finish();
}

/// #528 review: a malformed stored semantic enum is typed corruption, never healthy
/// availability. The store is reopened after the edit, as accidental corruption would be.
#[test]
fn malformed_semantic_freshness_or_state_fails_language_tiers() {
    fn index_db(dir: &Path) -> Option<PathBuf> {
        for entry in fs::read_dir(dir).ok()? {
            let path = entry.ok()?.path();
            if path.is_dir() {
                if let Some(found) = index_db(&path) {
                    return Some(found);
                }
            } else if path.file_name().is_some_and(|name| name == "index.db") {
                return Some(path);
            }
        }
        None
    }
    for corruption in [
        "UPDATE semantic_coverage_freshness SET freshness='garbage'",
        "UPDATE semantic_coverage SET state='garbage'",
    ] {
        let state = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let mut publisher = graph_fixtures::Publisher::open(state.path(), root.path());
        let mut spec = graph_fixtures::Spec::new(&[("src/lib.rs", "fn a(){b();} fn b(){}")]);
        spec.bindings = vec![graph_fixtures::bind("a", 0, "b")];
        publisher.publish(&spec);
        drop(publisher);
        // Healthy first: rust has fresh selected semantic evidence.
        let store = trellis::testing::retry_admission(|| {
            trellis::store::Store::open_for_tests(state.path(), root.path())
        });
        let tiers = store
            .evidence_response()
            .unwrap()
            .mcp_language_tiers()
            .unwrap();
        assert!(tiers[1].semantic(), "{tiers:?}");
        drop(store);
        let db = rusqlite::Connection::open(index_db(state.path()).unwrap()).unwrap();
        db.execute_batch(&format!(
            "PRAGMA ignore_check_constraints=ON; {corruption};"
        ))
        .unwrap();
        drop(db);
        let store = trellis::testing::retry_admission(|| {
            trellis::store::Store::open_for_tests(state.path(), root.path())
        });
        let error = store
            .evidence_response()
            .unwrap()
            .mcp_language_tiers()
            .unwrap_err();
        assert!(
            error.to_string().starts_with("store_unavailable:"),
            "{corruption}: {error:#}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_graph_eight_cases_through_mcp() {
    use graph_fixtures::{Bind, bind};
    use trellis::semantic_evidence::Dispatch;
    let files = [
        (
            "c1.rs",
            "fn c1_a() { c1_f(c1_g()); }\nfn c1_f(_x: i32) {}\nfn c1_g() {}\n",
        ),
        (
            "c2.rs",
            "fn c2_a() { c2_scheduler(C2::c2_run); }\nfn c2_scheduler() {}\nstruct C2;\nimpl C2 { fn c2_run() {} }\n",
        ),
        (
            "c3.rs",
            "fn c3_a() { c3_b(); }\nfn c3_b() { c3_a(); }\nfn c3_s() { c3_s(); }\n",
        ),
        (
            "c4.rs",
            "fn c4_a() { c4_b(); c4_c(); }\nfn c4_b() { c4_d(); }\nfn c4_c() { c4_d(); }\nfn c4_d() {}\n",
        ),
        (
            "c5.rs",
            "fn c5_a() { c5_b(); }\nfn c5_b() { c5_c(); }\nfn c5_c() {}\n",
        ),
        (
            "c6.rs",
            "fn c6_a() { c6_b(); c6_c(); c6_a(); }\nfn c6_b() {}\nfn c6_c() {}\n",
        ),
        (
            "c7.rs",
            "fn c7_a() { c7_b(); c7_c(); }\nfn c7_b() { c7_d(); }\nfn c7_c() {}\nfn c7_d() {}\nfn c7x_a() { c7x_b(); }\nfn c7x_b() {}\n",
        ),
        (
            "c8.rs",
            "fn c8_a() { c8_v(); }\nfn c8_v() {}\nfn c8_w() {}\n",
        ),
    ];
    let virtual_call = Bind {
        dispatch: Dispatch::Virtual,
        possible: vec!["c8_w".into()],
        ..bind("c8_a", 0, "c8_v")
    };
    let spec = with_bindings(
        &files,
        vec![
            bind("c1_a", 0, "c1_f"),
            bind("c1_a", 1, "c1_g"),
            bind("c2_a", 0, "c2_scheduler"),
            bind("c3_a", 0, "c3_b"),
            bind("c3_b", 0, "c3_a"),
            bind("c3_s", 0, "c3_s"),
            bind("c4_a", 0, "c4_b"),
            bind("c4_a", 1, "c4_c"),
            bind("c4_b", 0, "c4_d"),
            bind("c4_c", 0, "c4_d"),
            bind("c5_a", 0, "c5_b"),
            bind("c5_b", 0, "c5_c"),
            bind("c6_a", 0, "c6_b"),
            bind("c6_a", 1, "c6_c"),
            bind("c6_a", 2, "c6_a"),
            bind("c7_a", 0, "c7_b"),
            bind("c7_a", 1, "c7_c"),
            bind("c7_b", 0, "c7_d"),
            bind("c7x_a", 0, "c7x_b"),
            virtual_call,
        ],
    );
    let mut mcp = GraphMcp::start(311, &[spec], Default::default()).await;
    let producer = json!({"semanticProducerId":graph_fixtures::PRODUCER});
    let with = |extra: Value| {
        let mut merged = producer.clone();
        for (key, value) in extra.as_object().unwrap() {
            merged[key] = value.clone();
        }
        merged
    };
    // 1. Source order versus evaluation: the outer call sorts before the nested one.
    mcp.case(
        "c1_a",
        with(json!({})),
        &["c1_a:0", "c1_f:1", "c1_g:1"],
        &["c1_f:c1_a>c1_f:new:none", "c1_g:c1_a>c1_g:new:none"],
        &[],
        (false, false),
    );
    // 2. A callback reference is not a call.
    mcp.case(
        "c2_a",
        with(json!({"depth":1})),
        &["c2_a:0", "c2_scheduler:1"],
        &["c2_scheduler:c2_a>c2_scheduler:new:none"],
        &[],
        (false, false),
    );
    // 3. Cycle and self-loop.
    mcp.case(
        "c3_a",
        with(json!({})),
        &["c3_a:0", "c3_b:1"],
        &["c3_b:c3_a>c3_b:new:none", "c3_a:c3_b>c3_a:seen:none"],
        &[],
        (false, false),
    );
    mcp.case(
        "c3_s",
        with(json!({})),
        &["c3_s:0"],
        &["c3_s:c3_s>c3_s:seen:none"],
        &[],
        (false, false),
    );
    // Owner syntax identities include the workspace identity, so compare the actual
    // stable keys rather than assuming declaration names determine sibling order.
    let c4_b = mcp
        .names
        .iter()
        .find(|(_, name)| name.as_str() == "c4_b")
        .unwrap()
        .0;
    let c4_c = mcp
        .names
        .iter()
        .find(|(_, name)| name.as_str() == "c4_c")
        .unwrap()
        .0;
    let c4_edges = if c4_b < c4_c {
        ["c4_d:c4_b>c4_d:new:none", "c4_d:c4_c>c4_d:seen:none"]
    } else {
        ["c4_d:c4_c>c4_d:new:none", "c4_d:c4_b>c4_d:seen:none"]
    };
    // 4. Queued/visited diamond.
    mcp.case(
        "c4_a",
        with(json!({"depth":3})),
        &["c4_a:0", "c4_b:1", "c4_c:1", "c4_d:2"],
        &[
            "c4_b:c4_a>c4_b:new:none",
            "c4_c:c4_a>c4_c:new:none",
            c4_edges[0],
            c4_edges[1],
        ],
        &[],
        (false, false),
    );
    // 5. Depth.
    mcp.case(
        "c5_a",
        with(json!({"depth":1})),
        &["c5_a:0", "c5_b:1"],
        &["c5_b:c5_a>c5_b:new:none"],
        &["depth:c5_b:-:-:0:1"],
        (true, true),
    );
    mcp.case(
        "c5_a",
        with(json!({"depth":0})),
        &["c5_a:0"],
        &[],
        &["depth:c5_a:-:-:0:1"],
        (true, true),
    );
    // 6. Node cap with a later seen edge.
    mcp.case(
        "c6_a",
        with(json!({"maxNodes":2})),
        &["c6_a:0", "c6_b:1"],
        &[
            "c6_b:c6_a>c6_b:new:none",
            "c6_c:c6_a>-:boundary:nodeLimit",
            "c6_a:c6_a>c6_a:seen:none",
        ],
        &["nodeLimit:c6_a:c6_c:c6_c:-:0"],
        (true, true),
    );
    // 7. Global call cap, three sub-cases.
    mcp.case(
        "c7_a",
        with(json!({"maxCalls":1})),
        &["c7_a:0", "c7_b:1"],
        &["c7_b:c7_a>c7_b:new:none"],
        &["callLimit:c7_a:-:-:-:1", "callLimit:c7_b:-:-:-:1"],
        (true, true),
    );
    mcp.case(
        "c7_a",
        with(json!({"maxCalls":0})),
        &["c7_a:0"],
        &[],
        &["callLimit:c7_a:-:-:-:2"],
        (true, true),
    );
    mcp.case(
        "c7x_a",
        with(json!({"maxCalls":1})),
        &["c7x_a:0", "c7x_b:1"],
        &["c7x_b:c7x_a>c7x_b:new:none"],
        &[],
        (false, false),
    );
    // 8a. A virtual dispatch is a boundary that shows its declaration (#557); the possible
    // target is not admitted.
    mcp.case(
        "c8_a",
        with(json!({})),
        &["c8_a:0", "c8_v:1"],
        &["c8_v:c8_a>c8_v:boundary:dispatch"],
        &[],
        (false, true),
    );
    mcp.finish();
}

/// #466: an ambiguous call published as one row per provenance (sharing `call_id`) is one
/// ambiguous edge carrying the full candidate union, not `store_unavailable`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_graph_ambiguous_rows_sharing_a_call_through_mcp() {
    use graph_fixtures::{Bind, bind};
    use trellis::semantic_evidence::Resolution;
    let spec = with_bindings(
        &[("a.rs", "fn a() { b(); }\nfn b() {}\nfn c() {}\nfn d() {}\n")],
        [("amb-3", "d"), ("amb-1", "b"), ("amb-2", "c")]
            .into_iter()
            .map(|(provenance, target)| Bind {
                resolution: Resolution::Ambiguous,
                provenance: provenance.into(),
                ..bind("a", 0, target)
            })
            .collect(),
    );
    let mut mcp = GraphMcp::start(320, &[spec], Default::default()).await;
    let producer = json!({"semanticProducerId":graph_fixtures::PRODUCER,"includeExternal":true});
    for _ in 0..2 {
        let envelope = mcp.case(
            "a",
            producer.clone(),
            &["a:0"],
            &["b:a>-:boundary:ambiguous"],
            &[],
            (false, true),
        );
        let binding = &envelope["data"]["result"]["edges"][0]["binding"];
        assert_eq!(binding["resolution"], "ambiguous");
        assert_eq!(binding["provenanceId"], "amb-1");
        let mut internal: Vec<String> = binding["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["syntaxId"].as_str().map(|id| mcp.name(&json!(id))))
            .collect();
        internal.sort();
        assert_eq!(internal, ["b", "c", "d"]);
        assert_eq!(binding["candidates"].as_array().unwrap().len(), 4);
        let proofs: Vec<_> = envelope["data"]["result"]["provenance"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["producerId"] == graph_fixtures::PRODUCER)
            .map(|p| p["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(proofs, ["amb-1", "amb-2", "amb-3"]);
        assert_eq!(
            GraphMcp::warning_keys(&envelope),
            keys(&[("bindingAmbiguous", Some("amb-1"))])
        );
    }
    mcp.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_graph_five_warning_checks_through_mcp() {
    use graph_fixtures::{Bind, bind};
    use trellis::semantic_evidence::{CoverageState, Dispatch, Resolution};
    let producer = json!({"semanticProducerId":graph_fixtures::PRODUCER});
    let with = |extra: Value| {
        let mut merged = producer.clone();
        for (key, value) in extra.as_object().unwrap() {
            merged[key] = value.clone();
        }
        merged
    };

    // Check 1 (syntax only), over a syntax-only publication.
    let spec = graph_fixtures::Spec::syntax_only(&[("s1.rs", "fn s1_a() {}\n")]);
    let mut mcp = GraphMcp::start(312, &[spec], Default::default()).await;
    let envelope = mcp.case("s1_a", json!({}), &["s1_a:0"], &[], &[], (false, false));
    assert_eq!(
        GraphMcp::warning_keys(&envelope),
        keys(&[("syntaxOnly", None)])
    );
    mcp.finish();

    // Checks 2 and 5, over fresh evidence.
    let ambiguous = |nth| Bind {
        resolution: Resolution::Ambiguous,
        provenance: "pA".into(),
        ..bind("k2_a", nth, "k2_b")
    };
    let mut spec = with_bindings(
        &[
            ("k2.rs", "fn k2_a() { k2_b(); k2_b(); }\nfn k2_b() {}\n"),
            ("k5a.rs", "fn k5_a() { k5_b(); }\n"),
            ("k5b.rs", "fn k5_b() {}\n"),
            ("k5c.rs", "fn k5_c() { k5_d(); }\n"),
            ("k5d.rs", "fn k5_d() {}\n"),
        ],
        vec![
            ambiguous(0),
            ambiguous(1),
            bind("k5_a", 0, "k5_b"),
            bind("k5_c", 0, "k5_d"),
        ],
    );
    spec.states.insert("k5a.rs".into(), CoverageState::Partial);
    spec.states
        .insert("k5b.rs".into(), CoverageState::Unsupported);
    spec.states
        .insert("k5d.rs".into(), CoverageState::Unsupported);
    let mut mcp = GraphMcp::start(313, &[spec], Default::default()).await;
    // 2. Two ambiguous bindings share one provenance: one warning, and no coverage warning.
    let envelope = mcp.case(
        "k2_a",
        with(json!({})),
        &["k2_a:0"],
        &[
            "k2_b:k2_a>-:boundary:ambiguous",
            "k2_b:k2_a>-:boundary:ambiguous",
        ],
        &[],
        (false, true),
    );
    assert_eq!(
        GraphMcp::warning_keys(&envelope),
        keys(&[("bindingAmbiguous", Some("pA"))])
    );
    // 5. A selected partial row warns; an unselected unsupported row alone does not.
    let envelope = mcp.case(
        "k5_a",
        with(json!({})),
        &["k5_a:0", "k5_b:1"],
        &["k5_b:k5_a>k5_b:new:none"],
        &[],
        (false, true),
    );
    assert_eq!(
        GraphMcp::warning_keys(&envelope),
        keys(&[("coverageIncomplete", None)])
    );
    let envelope = mcp.case(
        "k5_c",
        with(json!({})),
        &["k5_c:0", "k5_d:1"],
        &["k5_d:k5_c>k5_d:new:none"],
        &[],
        (false, false),
    );
    assert!(GraphMcp::warning_keys(&envelope).is_empty());
    let rows = envelope["data"]["result"]["coverage"].as_array().unwrap();
    assert!(rows.iter().any(|row| {
        row["producerId"] == graph_fixtures::PRODUCER
            && row["state"] == "unsupported"
            && row["selected"] == false
    }));
    // A depth frontier alone is partial and truncated, with no warning.
    let envelope = mcp.case(
        "k5_c",
        with(json!({"depth":0})),
        &["k5_c:0"],
        &[],
        &["depth:k5_c:-:-:0:1"],
        (true, true),
    );
    assert!(GraphMcp::warning_keys(&envelope).is_empty());
    mcp.finish();

    // Case 8b and check 3: every proof of a different requested producer version is
    // possiblyStale, and `stale` precedes `dispatch`.
    let virtual_call = Bind {
        dispatch: Dispatch::Virtual,
        possible: vec!["c8b_w".into()],
        ..bind("c8b_a", 0, "c8b_v")
    };
    let named = |caller: &str, target: &str, id: &str| Bind {
        provenance: id.into(),
        ..bind(caller, 0, target)
    };
    let mut spec = with_bindings(
        &[
            (
                "c8b.rs",
                "fn c8b_a() { c8b_v(); c8b_d(); }\nfn c8b_v() {}\nfn c8b_w() {}\nfn c8b_d() {}\n",
            ),
            (
                "t3.rs",
                "fn t3_a() { t3_x(); t3_y(); t3_z(); }\nfn t3_x() {}\nfn t3_y() {}\nfn t3_z() {}\n",
            ),
        ],
        vec![
            virtual_call,
            bind("c8b_a", 1, "c8b_d"),
            Bind {
                provenance: "p1".into(),
                ..bind("t3_a", 0, "t3_x")
            },
            Bind {
                provenance: "p2".into(),
                ..bind("t3_a", 1, "t3_y")
            },
            named("t3_a", "t3_z", "p3"),
        ],
    );
    spec.requested_version = "v2".into();
    // `named` binds the first call; move the third binding to its own call.
    spec.bindings[4].nth = 2;
    let mut mcp = GraphMcp::start(314, &[spec], Default::default()).await;
    mcp.case(
        "c8b_a",
        with(json!({})),
        &["c8b_a:0"],
        &[
            "c8b_v:c8b_a>-:boundary:stale",
            "c8b_d:c8b_a>-:boundary:stale",
        ],
        &[],
        (false, true),
    );
    // 3. Three possiblyStale provenances give one aggregate warning.
    let envelope = mcp.case(
        "t3_a",
        with(json!({})),
        &["t3_a:0"],
        &[
            "t3_x:t3_a>-:boundary:stale",
            "t3_y:t3_a>-:boundary:stale",
            "t3_z:t3_a>-:boundary:stale",
        ],
        &[],
        (false, true),
    );
    assert_eq!(
        GraphMcp::warning_keys(&envelope),
        keys(&[("staleEvidence", None)])
    );
    for id in ["p1", "p2", "p3"] {
        let proof = envelope["data"]["result"]["provenance"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .unwrap_or_else(|| panic!("missing provenance {id}"));
        assert_eq!(proof["freshness"], "possiblyStale");
    }
    mcp.finish();

    // 4. Mixed freshness with a stale boundary, over two publications: r1 declares x and z;
    // at r2 the producer is omitted for both documents, x's document changed and z's did not.
    let mut r1 = graph_fixtures::Spec::new(&[
        ("d1.rs", "fn k4_root() { k4_x(); k4_z(); }\n"),
        ("d2.rs", "fn k4_x() {}\n"),
        ("d4.rs", "fn k4_z() {}\n"),
    ]);
    r1.declaration_bindings = vec!["k4_x".into(), "k4_z".into()];
    r1.declaration_proofs = [("k4_x", "p2"), ("k4_z", "p1")]
        .into_iter()
        .map(|(name, id)| (name.to_owned(), id.to_owned()))
        .collect();
    let mut r2 = with_bindings(
        &[
            ("d1.rs", "fn k4_root() { k4_x(); k4_z(); }\n"),
            ("d2.rs", "fn k4_x() { k4_w(); }\nfn k4_w() {}\n"),
            ("d4.rs", "fn k4_z() {}\n"),
        ],
        vec![bind("k4_root", 0, "k4_x"), bind("k4_root", 1, "k4_z")],
    );
    r2.states.insert("d2.rs".into(), CoverageState::Omitted);
    r2.states.insert("d4.rs".into(), CoverageState::Omitted);
    let mut mcp = GraphMcp::start(315, &[r1, r2], Default::default()).await;
    let envelope = mcp.case(
        "k4_root",
        with(json!({})),
        &["k4_root:0", "k4_x:1", "k4_z:1"],
        &[
            "k4_x:k4_root>k4_x:new:none",
            "k4_z:k4_root>k4_z:new:none",
            "k4_w:k4_x>-:boundary:missingEvidence",
        ],
        &[],
        (false, true),
    );
    assert_eq!(
        GraphMcp::warning_keys(&envelope),
        keys(&[("staleEvidence", None), ("staleEvidence", Some("p2"))])
    );
    let freshness = |id: &str| {
        envelope["data"]["result"]["provenance"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == id)
            .map(|p| p["freshness"].clone())
    };
    assert_eq!(freshness("p1"), Some(json!("possiblyStale")));
    assert_eq!(freshness("p2"), Some(json!("stale")));
    mcp.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_graph_oversized_answer_is_byte_reduced_and_valid() {
    let spec =
        graph_fixtures::Spec::syntax_only(&[("fan.rs", &graph_fixtures::fanout_source(600))]);
    let mut mcp = GraphMcp::start(316, &[spec], Default::default()).await;
    // #665: the default limits fit the response; ask for the contract maximum.
    let arguments = mcp.args("root", json!({"maxNodes":150,"maxCalls":500}));
    let id = mcp.send(arguments.clone());
    let mut line = String::new();
    mcp.reader.read_line(&mut line).unwrap();
    assert!(line.ends_with('\n'));
    assert!(
        line.len() <= trellis::mcp::wire::RESPONSE_BYTES,
        "frame is {} bytes",
        line.len()
    );
    let reply: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(reply["id"], id);
    tool(&reply, None, true);
    let envelope = mcp.record(&reply);
    let result = &envelope["data"]["result"];
    let k = result["edges"].as_array().unwrap().len();
    assert!(0 < k && k < 500, "byte prefix has {k} groups");
    assert_eq!(result["request"]["maxCalls"], 500);
    assert_eq!(envelope["data"]["requested"]["maxCalls"], 500);
    assert_eq!(envelope["truncationReason"], "response_bytes");
    assert_eq!(envelope["truncated"], true);
    assert_eq!(envelope["partial"], true);
    assert_eq!(result["omitted"]["callLimit"], 100);
    assert_eq!(result["omitted"]["responseBytes"], 500 - k);
    assert_eq!(
        envelope["data"]["responseOmitted"],
        json!({"nodes":0,"edges":500 - k})
    );
    assert_eq!(*result, mcp.store_answer(&arguments, Some(k)));
    assert!(
        result["frontier"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["nextOrdinal"].is_null())
    );
    // The next prefix is the first one the forward pass refuses.
    let next = mcp.store_answer(&arguments, Some(k + 1));
    let mut longer = envelope.clone();
    longer["data"]["result"] = next;
    let frame = json!({"jsonrpc":"2.0","id":id,
        "result":{"isError":false,"structuredContent":longer,
            "content":[{"type":"text","text":reply["result"]["content"][0]["text"]}],
            "resultType":"complete"}});
    assert!(serde_json::to_vec(&frame).unwrap().len() + 1 > trellis::mcp::wire::RESPONSE_BYTES);
    // A caller's own smaller cap is honoured and not reported as a byte reduction.
    let envelope = mcp.graph(mcp.args("root", json!({"maxCalls":3})));
    assert_eq!(
        envelope["data"]["result"]["edges"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(envelope["truncationReason"], "limit");
    assert_eq!(envelope["data"]["requested"]["maxCalls"], 3);
    mcp.finish();
}

fn index_db(state: &Path) -> PathBuf {
    fs::read_dir(state.join("cache/indexes"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap()
        .join("index.db")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn call_graph_basis_conflict_and_failure_codes() {
    let binding = graph_fixtures::bind("fc_a", 0, "fc_b");
    let r1 = with_bindings(
        &[("a.rs", "fn fc_a() { fc_b(); }\nfn fc_b() {}\n")],
        vec![binding.clone()],
    );
    let r2 = with_bindings(
        &[(
            "a.rs",
            "fn fc_a() { fc_b(); }\nfn fc_b() {}\nfn fc_c() {}\n",
        )],
        vec![binding],
    );
    let mut mcp = GraphMcp::start(317, &[r1, r2], Default::default()).await;
    let pin = |pin: trellis::model::IndexPin| json!({"indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision});
    let (old, head) = (pin(mcp.pins[0]), pin(mcp.pins[1]));
    assert_ne!(old, head);
    let producer = json!({"semanticProducerId":graph_fixtures::PRODUCER});
    // The admitted head: the basis, the requested revision and the resolved revision agree.
    let envelope = mcp.graph(mcp.args(
        "fc_a",
        json!({"semanticProducerId":graph_fixtures::PRODUCER,"expectedBasis":head}),
    ));
    assert_eq!(envelope["evidenceBasis"], head);
    let sql_revision: String = rusqlite::Connection::open(index_db(mcp.temp.path()))
        .unwrap()
        .query_row(
            "SELECT native_revision_id FROM native_revisions ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        envelope["data"]["result"]["resolvedRevisionId"],
        sql_revision
    );
    assert_eq!(envelope["data"]["requested"]["revisionId"], sql_revision);
    assert_eq!(
        envelope["data"]["result"]["request"]["revisionId"],
        sql_revision
    );
    // A retained older pin is a conflict, never served.
    let conflict = mcp.failure(
        mcp.args("fc_a", json!({"expectedBasis":old})),
        "revision_conflict",
    );
    assert!(conflict.get("data").is_none());
    // Contract failures keep the shared code set and carry their own message.
    let missing = format!("sid:v1:{}", "0".repeat(32));
    let mut arguments = mcp.args("fc_a", producer.clone());
    arguments["symbolId"] = json!(missing);
    let envelope = mcp.failure(arguments, "not_found");
    assert_eq!(
        envelope["error"]["message"],
        "Call graph root not found in the admitted snapshot"
    );
    let envelope = mcp.failure(
        mcp.args("fc_a", json!({"semanticProducerId":"semantic.unpublished"})),
        "not_found",
    );
    assert_eq!(
        envelope["error"]["message"],
        "Semantic producer not published for the admitted revision"
    );
    // Out-of-range input is invalid, never clamped.
    for extra in [
        json!({"depth":6}),
        json!({"maxNodes":0}),
        json!({"maxNodes":151}),
        json!({"maxCalls":501}),
        json!({"semanticProducerId":""}),
        json!({"unknown":1}),
    ] {
        mcp.failure(mcp.args("fc_a", extra), "invalid_request");
    }
    mcp.finish();
}

/// Runs a child test binary with the phase hook armed in the child's environment only.
fn run_hook_child(test: &str) {
    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            child_test_name(test).as_str(),
            "--ignored",
            "--test-threads=1",
            "--nocapture",
        ])
        .env("TRELLIS_TEST_MCP_PHASE", "call_graph_node")
        .env(
            "TRELLIS_TEST_MCP_PHASE_SOCKET",
            dir.path().join("phase.sock"),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = std::time::Instant::now() + trellis::testing::HANG_BACKSTOP;
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let log = format!("{}\n{}", out.join().unwrap(), err.join().unwrap());
    assert!(
        status.is_some_and(|status| status.success()),
        "{test} failed:\n{log}"
    );
}

/// The hook child binds the phase socket itself, so nothing outside it is ever armed.
fn bind_phase_socket() -> std::os::unix::net::UnixListener {
    let path = std::env::var_os("TRELLIS_TEST_MCP_PHASE_SOCKET").expect("run by its parent test");
    std::os::unix::net::UnixListener::bind(path).unwrap()
}

/// Accepts one hook connection from the call graph and returns the stream held at the hook.
fn hold_at_node_hook(
    listener: &std::os::unix::net::UnixListener,
) -> std::os::unix::net::UnixStream {
    let gate = trellis::testing::accept_within_backstop(listener);
    let mut received = String::new();
    BufReader::new(gate.try_clone().unwrap())
        .read_line(&mut received)
        .unwrap();
    assert_eq!(received, "call_graph_node\n");
    gate
}

fn hook_spec() -> graph_fixtures::Spec {
    with_bindings(
        &[("a.rs", "fn hk_a() { hk_b(); }\nfn hk_b() {}\n")],
        vec![graph_fixtures::bind("hk_a", 0, "hk_b")],
    )
}

#[test]
fn call_graph_deadline_at_node_hook() {
    run_hook_child("call_graph_deadline_child");
}

#[test]
#[ignore = "runs only as the child of call_graph_deadline_at_node_hook"]
fn call_graph_deadline_child() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = bind_phase_socket();
            let mut mcp = GraphMcp::start(318, &[hook_spec()], Default::default()).await;
            let arguments = mcp.args(
                "hk_a",
                json!({"semanticProducerId":graph_fixtures::PRODUCER}),
            );
            let id = mcp.send(arguments);
            let mut gate = hold_at_node_hook(&listener);
            mcp.runtime
                .advance_mcp_clock_for_tests(Duration::from_millis(5001));
            gate.write_all(b"!").unwrap();
            let reply = next_reply(&mut mcp.reader);
            assert_eq!(reply["id"], id);
            tool(&reply, Some("deadline_exceeded"), true);
            mcp.record(&reply);
            mcp.finish();
        });
}

#[test]
fn call_graph_cancel_at_node_hook_is_suppressed() {
    run_hook_child("call_graph_cancel_child");
}

#[test]
#[ignore = "runs only as the child of call_graph_cancel_at_node_hook_is_suppressed"]
fn call_graph_cancel_child() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let listener = bind_phase_socket();
            let (read_tx, read_rx) = mpsc::sync_channel(1);
            let frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let hooks = trellis::mcp::TransportHooksForTests {
                frame_read: Some(std::sync::Arc::new(move || {
                    // Frames: the startup describe, the graph call, then the cancellation.
                    if frames.fetch_add(1, std::sync::atomic::Ordering::AcqRel) == 2 {
                        let _ = read_tx.try_send(());
                    }
                })),
                ..Default::default()
            };
            let mut mcp = GraphMcp::start(319, &[hook_spec()], hooks).await;
            let arguments = mcp.args("hk_a", json!({"semanticProducerId":graph_fixtures::PRODUCER}));
            let id = mcp.send(arguments);
            let mut gate = hold_at_node_hook(&listener);
            writeln!(
                mcp.client,
                "{}",
                json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})
            )
            .unwrap();
            read_rx
                .recv_timeout(trellis::testing::HANG_BACKSTOP)
                .unwrap();
            gate.write_all(b"!").unwrap();
            // The cancelled call is suppressed, and the next call on the connection is answered.
            writeln!(mcp.client, "{}", describe_call(90)).unwrap();
            let reply = next_reply(&mut mcp.reader);
            assert_eq!(reply["id"], 90, "cancelled call emitted a reply: {reply}");
            tool(&reply, None, true);
            mcp.finish();
        });
}

// #593: exercise the wire contract against real indexed Rust declarations, not hand-made IDs.
const LOOKUP_ROOT_TOOLS: [&str; 4] = [
    "trellis_declaration",
    "trellis_outgoing_calls",
    "trellis_incoming_calls",
    "trellis_call_graph",
];

fn lookup_reply(mcp: &mut GraphMcp, name: &str, arguments: Value, code: Option<&str>) -> Value {
    mcp.next_id += 1;
    let id = mcp.next_id;
    writeln!(
        mcp.client,
        "{}",
        modern(
            json!(id),
            "tools/call",
            json!({"name":name,"arguments":arguments})
        )
    )
    .unwrap();
    let reply = next_reply(&mut mcp.reader);
    assert_eq!(reply["id"], id);
    tool(&reply, code, true);
    let envelope = reply["result"]["structuredContent"].clone();
    mcp.pairs
        .push(json!({"name":name,"structuredContent":envelope}));
    envelope
}

fn lookup_basis(pin: trellis::model::IndexPin) -> Value {
    json!({"indexGeneration":pin.index_generation.to_string(),"indexRevision":pin.index_revision})
}

fn assert_lookup_symbol_closure(data: &Value, field: &str, count: usize) {
    let symbols = data[field].as_array().unwrap();
    assert_eq!(symbols.len(), count);
    assert!(!data["scopeCoverage"].as_array().unwrap().is_empty());
    let documents = data["documents"].as_array().unwrap();
    let coverage = data["coverage"].as_array().unwrap();
    let provenance = data["provenance"].as_array().unwrap();
    assert_eq!(documents.len(), 1);
    assert_eq!(coverage.len(), 1);
    assert_eq!(provenance.len(), 1);
    for symbol in symbols {
        let document = emitted_document(data, symbol);
        assert_eq!(document["document"]["path"], "src/lib.rs");
        assert!(symbol["lines"]["start"].as_u64().is_some());
        assert!(
            symbol["range"]["end"].as_u64().unwrap() > symbol["range"]["start"].as_u64().unwrap()
        );
        assert_eq!(symbol["evidenceTier"], "syntax");
        assert!(
            coverage
                .iter()
                .any(|row| row["documentPath"] == "src/lib.rs")
        );
        assert!(
            provenance
                .iter()
                .any(|row| row["document"] == document["document"])
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stable_lookup_recovers_moved_impl_without_changing_sid_semantics() {
    let first = graph_fixtures::Spec::syntax_only(&[(
        "src/lib.rs",
        "struct Store;\nimpl Store { fn run() { helper(); } }\nfn helper() {}\nfn caller() { Store::run(); }\n",
    )]);
    let moved = graph_fixtures::Spec::syntax_only(&[(
        "src/lib.rs",
        "struct Other;\nimpl Other { fn unrelated() {} }\nstruct Store;\nimpl Store { fn run() { helper(); } }\nfn helper() {}\nfn caller() { Store::run(); }\n",
    )]);
    let mut mcp = GraphMcp::start(5931, &[first, moved], Default::default()).await;
    let old_id = mcp.revision_names[0]
        .iter()
        .find(|(_, name)| name.as_str() == "run")
        .unwrap()
        .0
        .clone();
    let current_id = mcp.id_of("run");
    assert_ne!(
        old_id, current_id,
        "the unrelated impl changes the SID owner ordinal"
    );
    let key = "src/lib.rs::Store::run";
    let basis = lookup_basis(mcp.pins[1]);
    let found = mcp.call_tool(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"run","expectedBasis":basis}),
    );
    let symbol = &found["data"]["symbols"][0];
    assert_eq!(symbol["symbolId"], current_id);
    assert_eq!(symbol["stableLookupKey"], key);
    let exact = mcp.call_tool(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"lookupKey":key,"expectedBasis":basis}),
    );
    assert_eq!(exact["data"], found["data"]);
    let search = mcp.call_tool(
        "trellis_search_symbols",
        json!({"schemaVersion":1,"query":"run","expectedBasis":basis}),
    );
    let hit = search["data"]["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["symbolId"] == current_id)
        .unwrap();
    assert_eq!(hit["stableLookupKey"], key);
    for name in LOOKUP_ROOT_TOOLS {
        lookup_reply(
            &mut mcp,
            name,
            json!({"schemaVersion":1,"symbolId":old_id,"expectedBasis":basis}),
            Some("not_found"),
        );
        let by_key = mcp.call_tool(
            name,
            json!({"schemaVersion":1,"lookupKey":key,"expectedBasis":basis}),
        );
        let by_id = mcp.call_tool(
            name,
            json!({"schemaVersion":1,"symbolId":current_id,"expectedBasis":basis}),
        );
        assert_eq!(by_key["data"], by_id["data"], "{name}");
        assert_eq!(by_key["data"]["stableLookupKey"], key, "{name}");
        assert_eq!(by_key["evidenceBasis"], basis);
        match name {
            "trellis_declaration" => {
                assert_eq!(by_key["data"]["declaration"]["syntaxId"], current_id)
            }
            "trellis_outgoing_calls" => {
                assert_eq!(by_key["data"]["calls"].as_array().unwrap().len(), 1)
            }
            "trellis_incoming_calls" => {
                assert_eq!(by_key["data"]["callers"].as_array().unwrap().len(), 1)
            }
            "trellis_call_graph" => assert_eq!(
                by_key["data"]["result"]["request"]["rootSyntaxId"],
                current_id
            ),
            _ => unreachable!(),
        }
    }
    mcp.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stable_lookup_preserves_delimiters_in_source_paths() {
    let spec = graph_fixtures::Spec::syntax_only(&[(
        "src::lib.rs",
        "struct Store;\nimpl Store { fn run() {} }\n",
    )]);
    let mut mcp = GraphMcp::start(5934, &[spec], Default::default()).await;
    let id = mcp.id_of("run");
    let key = "src::lib.rs::Store::run";
    let found = mcp.call_tool(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"run"}),
    );
    assert_eq!(found["data"]["symbols"].as_array().unwrap().len(), 1);
    let symbol = &found["data"]["symbols"][0];
    assert_eq!(symbol["symbolId"], id);
    assert_eq!(symbol["stableLookupKey"], key);
    assert_eq!(
        emitted_document(&found["data"], symbol)["document"]["path"],
        "src::lib.rs"
    );
    let exact = mcp.call_tool(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"lookupKey":key}),
    );
    assert_eq!(exact["data"], found["data"]);
    let by_key = mcp.call_tool(
        "trellis_declaration",
        json!({"schemaVersion":1,"lookupKey":key}),
    );
    let by_id = mcp.call_tool(
        "trellis_declaration",
        json!({"schemaVersion":1,"symbolId":id}),
    );
    assert_eq!(by_key["data"], by_id["data"]);
    assert_eq!(by_key["data"]["declaration"]["syntaxId"], id);
    assert_eq!(by_key["data"]["stableLookupKey"], key);
    mcp.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stable_lookup_ambiguity_returns_all_candidates_and_never_runs_the_operation() {
    let spec = graph_fixtures::Spec::syntax_only(&[(
        "src/lib.rs",
        "struct Foo;\nimpl Foo { fn run() {} }\nimpl Foo { fn run() {} }\n",
    )]);
    let mut mcp = GraphMcp::start(5932, &[spec], Default::default()).await;
    let key = "src/lib.rs::Foo::run";
    let expected_ids: std::collections::BTreeSet<_> = mcp
        .names
        .iter()
        .filter(|(_, name)| name.as_str() == "run")
        .map(|(id, _)| json!(id).to_string())
        .collect();
    assert_eq!(expected_ids.len(), 2);
    let found = mcp.call_tool(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"lookupKey":key,"limit":1}),
    );
    assert_lookup_symbol_closure(&found["data"], "symbols", 2);
    assert_eq!(found["truncated"], false);
    for name in LOOKUP_ROOT_TOOLS {
        let result = mcp.call_tool(name, json!({"schemaVersion":1,"lookupKey":key}));
        let data = &result["data"];
        assert_eq!(data["lookupKey"], key);
        assert_lookup_symbol_closure(data, "candidates", 2);
        assert_eq!(data["candidates"], found["data"]["symbols"]);
        let ids: std::collections::BTreeSet<_> = data["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|candidate| {
                assert_eq!(candidate["stableLookupKey"], key);
                candidate["symbolId"].to_string()
            })
            .collect();
        assert_eq!(ids, expected_ids);
        for operation_field in [
            "declaration",
            "calls",
            "callers",
            "result",
            "requested",
            "symbolId",
        ] {
            assert!(
                data.get(operation_field).is_none(),
                "{name} executed: {data}"
            );
        }
        assert_eq!(result["truncated"], false);
        assert!(result["truncationReason"].is_null());
        assert_eq!(result["evidenceBasis"], found["evidenceBasis"]);
    }
    mcp.finish();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stable_lookup_selector_validation_missing_and_revision_conflict() {
    let old = graph_fixtures::Spec::syntax_only(&[("src/lib.rs", "fn run() {}\n")]);
    let head = graph_fixtures::Spec::syntax_only(&[("src/lib.rs", "fn run() {}\nfn added() {}\n")]);
    let mut mcp = GraphMcp::start(5933, &[old, head], Default::default()).await;
    let id = mcp.id_of("run");
    let basis = lookup_basis(mcp.pins[1]);
    let old_basis = lookup_basis(mcp.pins[0]);
    assert_ne!(basis, old_basis);
    let missing = "src/lib.rs::missing";
    let empty = mcp.call_tool(
        "trellis_find_symbols",
        json!({"schemaVersion":1,"lookupKey":missing,"expectedBasis":basis}),
    );
    for field in ["symbols", "documents", "coverage", "provenance"] {
        assert_eq!(empty["data"][field], json!([]));
    }
    assert_eq!(empty["truncated"], false);
    for name in LOOKUP_ROOT_TOOLS
        .into_iter()
        .chain(["trellis_find_symbols"])
    {
        let selector = if name == "trellis_find_symbols" {
            "query"
        } else {
            "symbolId"
        };
        let primary = if selector == "query" {
            json!("run")
        } else {
            json!(id)
        };
        let mut both = json!({"schemaVersion":1,"lookupKey":"src/lib.rs::run"});
        both[selector] = primary;
        for arguments in [
            both,
            json!({"schemaVersion":1}),
            json!({"schemaVersion":1,"lookupKey":null}),
            json!({"schemaVersion":1,"lookupKey":7}),
            json!({"schemaVersion":1,"lookupKey":""}),
        ] {
            lookup_reply(&mut mcp, name, arguments, Some("invalid_request"));
        }
        let conflict = lookup_reply(
            &mut mcp,
            name,
            json!({"schemaVersion":1,"lookupKey":"src/lib.rs::run","expectedBasis":old_basis}),
            Some("revision_conflict"),
        );
        assert!(conflict.get("data").is_none());
        if name != "trellis_find_symbols" {
            lookup_reply(
                &mut mcp,
                name,
                json!({"schemaVersion":1,"lookupKey":missing,"expectedBasis":basis}),
                Some("not_found"),
            );
        }
    }
    mcp.finish();
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// AC11 end to end (#683): TypeScript documents are first-class MCP citizens on a real index
/// of the mixed TypeScript/JavaScript fixture.
#[test]
fn typescript_documents_are_first_class() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    copy_tree(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/typescript-mixed"),
        &root,
    );
    let mut peer = Peer::start(Some(&root));
    ready_legacy(&mut peer);
    let home = peer._home.path().to_path_buf();
    let mut command = trellis::testing::owned(Command::new(env!("CARGO_BIN_EXE_trellis")));
    command
        .arg("index")
        .arg("--workspace")
        .arg(&root)
        .env("HOME", &home)
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("XDG_DATA_HOME", home.join("data"));
    let indexed = bounded_output(command);
    crate::child::assert_cli_success(&indexed, "index");
    let mut pairs = Vec::new();
    let mut next_id = 0;
    let mut ask = |peer: &mut Peer, name: &str, arguments: Value| {
        next_id += 1;
        let reply = peer.ask(call(true, json!(next_id), name, arguments));
        tool(&reply, None, false);
        let envelope = reply["result"]["structuredContent"].clone();
        pairs.push(json!({"name":name,"structuredContent":envelope}));
        envelope
    };
    let languages = |envelope: &Value| -> Vec<String> {
        envelope["data"]["scopeCoverage"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["language"].as_str().unwrap().to_owned())
            .collect()
    };
    let described = ask(
        &mut peer,
        "trellis_workspace_describe",
        json!({"schemaVersion":1}),
    );
    let rows = described["data"]["languages"].as_array().unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[3]["language"], "javascript");
    assert_eq!(rows[4]["language"], "typescript");
    assert!(rows[4]["coverage"]["selected"].as_u64().unwrap() > 0);

    let document_languages = |envelope: &Value| -> Vec<String> {
        let mut found: Vec<String> = envelope["data"]["documents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["document"]["language"].as_str().unwrap().to_owned())
            .collect();
        found.sort();
        found.dedup();
        found
    };
    let model = ask(
        &mut peer,
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"Model"}),
    );
    assert_eq!(document_languages(&model), ["typescript"]);
    assert_eq!(languages(&model), ["typescript"]);
    let symbol = model["data"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "Model")
        .expect("the Model class");
    let helper = ask(
        &mut peer,
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"helper"}),
    );
    assert_eq!(languages(&helper), ["javascript", "typescript"]);

    let declaration = ask(
        &mut peer,
        "trellis_declaration",
        json!({"schemaVersion":1,"symbolId":symbol["symbolId"]}),
    );
    assert_eq!(
        declaration["data"]["documents"][0]["document"]["language"],
        "typescript"
    );
    assert_eq!(declaration["data"]["displayKey"], "src/app/model.ts#Model");
    let source = ask(
        &mut peer,
        "trellis_read_source",
        json!({"schemaVersion":1,"path":"src/app/model.ts","startLine":1,"endLine":4}),
    );
    assert!(
        source["data"]["text"]
            .as_str()
            .is_some_and(|text| text.contains("export interface Entity"))
    );
    let references = ask(
        &mut peer,
        "trellis_references",
        json!({"schemaVersion":1,"paths":["src/app/**"]}),
    );
    let modules = references["data"]["modules"].as_array().unwrap();
    assert!(modules.iter().any(|module| {
        module["identity"]["language"] == "typescript"
            && module["identity"]["path"] == "src/app/bridge.ts"
    }));
    let bridge = ask(
        &mut peer,
        "trellis_find_symbols",
        json!({"schemaVersion":1,"query":"bridge"}),
    );
    let root = bridge["data"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|symbol| symbol["name"] == "bridge")
        .expect("the bridge function");
    let graph = ask(
        &mut peer,
        "trellis_call_graph",
        json!({"schemaVersion":1,"symbolId":root["symbolId"]}),
    );
    assert_eq!(
        graph["data"]["result"]["coverage"][0]["language"],
        "typescript"
    );
    assert_eq!(
        graph["data"]["result"]["edges"][0]["call"]["document"]["path"],
        "src/app/bridge.ts"
    );
    validate_structured_content(&pairs);
    peer.assert_success("trellis mcp");
}

/// #683: the rules tools accept `typescript` as a module language and npm package language,
/// and still reject the miscased name as an invalid request.
#[test]
fn rules_explain_accepts_typescript_targets_and_npm_destinations() {
    let temp = tempfile::tempdir().unwrap();
    let root = checkout(temp.path());
    let mut peer = Peer::start(Some(&root));
    ready_legacy(&mut peer);
    let target = |language: &str| {
        json!({"sourceSetId":"app","language":language,"path":"src/app/model.ts",
            "sid":"sid:v1:00000000000000000000000000000000"})
    };
    let npm = json!({"kind":"externalPackage","sourceSetId":"app","language":"typescript",
        "package":"@scope/library","standardLibrary":false});
    let mut explain = |id: i64, language: &str, destination: Value| -> Value {
        peer.ask(call(
            true,
            json!(id),
            "trellis_rules_explain",
            json!({"schemaVersion":1,"rule":"$external","target":target(language),
                "destination":destination}),
        ))
    };
    let accepted = explain(1, "typescript", npm.clone());
    // Validation passed, so the call reached the store, which has no index yet.
    tool(&accepted, Some("index_not_ready"), false);
    let rejected = explain(2, "TypeScript", npm);
    tool(&rejected, Some("invalid_request"), false);
    let rejected = explain(
        3,
        "typescript",
        json!({"kind":"externalPackage","sourceSetId":"app","language":"typescript",
            "package":"@scope/library*","standardLibrary":false}),
    );
    tool(&rejected, Some("invalid_request"), false);
    peer.assert_success("trellis mcp");
}

/// A child test's libtest name in the grouped test binary, which names this
/// module's tests `<module>::<test>`.
fn child_test_name(test: &str) -> String {
    format!("{}::{test}", module_path!().split_once("::").unwrap().1)
}

/// #780: describe pages the documents behind each language's semantic failed and partial
/// counts, with typed reasons, and omits the list unless asked. Every answer, including the
/// typed failures, fits the catalog schema.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn describe_pages_failed_and_partial_semantic_documents() {
    use trellis::semantic_evidence::CoverageState;
    let mut spec = graph_fixtures::Spec::new(&[
        ("src/a.rs", "fn a() {}\n"),
        ("src/b.rs", "fn b() {}\n"),
        ("src/c.rs", "fn c() {}\n"),
        ("src/d.rs", "fn d() {}\n"),
    ]);
    for (path, state) in [
        ("src/b.rs", CoverageState::Failed),
        ("src/c.rs", CoverageState::Failed),
        ("src/d.rs", CoverageState::Partial),
    ] {
        spec.states.insert(path.into(), state);
    }
    spec.diagnostics
        .insert("src/c.rs".into(), "excluded_failed_by_producer".into());
    let mut mcp = GraphMcp::start(780, &[spec], Default::default()).await;
    let describe = |mcp: &mut GraphMcp, request: Option<Value>| {
        let mut arguments = json!({"schemaVersion":1});
        if let Some(request) = request {
            arguments["coverageDocuments"] = request;
        }
        mcp.call_tool("trellis_workspace_describe", arguments)
    };
    // Negative control: a plain describe carries no document list.
    let plain = describe(&mut mcp, None);
    assert!(plain["data"].get("coverageDocuments").is_none(), "{plain}");
    let rust = plain["data"]["languages"][1]["semantic"]["coverage"].clone();
    assert_eq!(
        (rust["failed"].as_u64(), rust["partial"].as_u64()),
        (Some(2), Some(1))
    );
    let all = describe(&mut mcp, Some(json!({})));
    assert_eq!(all["data"]["languages"], plain["data"]["languages"]);
    assert_eq!(
        all["data"]["coverageDocuments"],
        json!({"items":[
            {"language":"rust","path":"src/b.rs","state":"failed","reasons":["other"]},
            {"language":"rust","path":"src/c.rs","state":"failed","reasons":["notInProducerOutput"]},
            {"language":"rust","path":"src/d.rs","state":"partial","reasons":["unrecorded"]},
        ],"page":{"offset":0,"nextOffset":null,"total":3}})
    );
    // One document per page concatenates to the same list.
    let mut offset = json!(0);
    let mut paged = Vec::new();
    loop {
        let page = describe(
            &mut mcp,
            Some(json!({"language":"rust","limit":1,"offset":offset})),
        );
        let documents = &page["data"]["coverageDocuments"];
        assert_eq!(documents["items"].as_array().unwrap().len(), 1);
        paged.extend(documents["items"].as_array().unwrap().iter().cloned());
        offset = documents["page"]["nextOffset"].clone();
        if offset.is_null() {
            break;
        }
    }
    assert_eq!(json!(paged), all["data"]["coverageDocuments"]["items"]);
    let python = describe(&mut mcp, Some(json!({"language":"python"})));
    assert_eq!(
        python["data"]["coverageDocuments"],
        json!({"items":[],"page":{"offset":0,"nextOffset":null,"total":0}})
    );
    for (request, code) in [
        (json!({"offset":4}), "range_too_large"),
        (json!({"language":"go"}), "invalid_request"),
        (json!({"limit":51}), "range_too_large"),
    ] {
        mcp.next_id += 1;
        let id = mcp.next_id;
        writeln!(
            mcp.client,
            "{}",
            modern(
                json!(id),
                "tools/call",
                json!({"name":"trellis_workspace_describe",
                    "arguments":{"schemaVersion":1,"coverageDocuments":request}})
            )
        )
        .unwrap();
        let reply = next_reply(&mut mcp.reader);
        tool(&reply, Some(code), true);
        let envelope = reply["result"]["structuredContent"].clone();
        if code == "range_too_large" && request.get("offset").is_some() {
            assert_eq!(envelope["error"]["currentBasis"], all["evidenceBasis"]);
        }
        mcp.pairs
            .push(json!({"name":"trellis_workspace_describe","structuredContent":envelope}));
    }
    mcp.finish();
}
