//! Clients during an explicit `trellis daemon stop` (#1293, owner ruling
//! 2026-11-26): the stopping daemon answers new connections with the typed,
//! retryable `daemon_stopping`; CLI, `serve` and `trellis mcp` attach wait for
//! it and reach the successor, and a tool call inside an MCP session answers it
//! within its own deadline. Each stop is held open by a daemon hook.
use crate::daemon_stop::{Fixture, Proc, arrival, fixture, release, stop_reaching_runtimes};
use std::fs;
use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::Stdio;
use std::time::{Duration, Instant};
use trellis::testing::HANG_BACKSTOP;

/// A daemon whose stop is held open: `index` activated the checkout, its
/// startup reconciliation is held by the hook, and the stop command has
/// reached the runtime. New connections now meet the stopping daemon.
struct HeldStop {
    daemon: Proc,
    stop: Proc,
    held: UnixStream,
    stopping: UnixListener,
}

fn held_stop(fixture: &Fixture) -> HeldStop {
    let (startup, startup_path) = fixture.hook("startup.sock");
    let (stop_hook, stop_path) = fixture.hook("stop.sock");
    let (stopping, stopping_path) = fixture.hook("stopping.sock");
    let daemon = fixture.daemon(&[
        ("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path),
        ("TRELLIS_TEST_DAEMON_STOP_SOCKET", &stop_path),
        ("TRELLIS_TEST_DAEMON_STOPPING_SOCKET", &stopping_path),
    ]);
    let mut index = fixture.cli();
    index.arg("index").arg("--workspace").arg(&fixture.root);
    let mut index = fixture.spawn("index", index);
    let held = arrival(&startup, 1);
    let stop = stop_reaching_runtimes(fixture, &stop_hook);
    assert!(!index.exit("index during the stop").success());
    HeldStop {
        daemon,
        stop,
        held,
        stopping,
    }
}

impl HeldStop {
    /// The stopping daemon answered one connection `daemon_stopping`.
    fn answered(&self) {
        drop(arrival(&self.stopping, b'A'));
    }

    /// Release the held work and let the stop finish.
    fn finish(mut self) {
        release(self.held);
        assert!(
            self.stop.exit("daemon stop").success(),
            "{}",
            self.stop.stderr()
        );
        assert!(self.daemon.exit("stopped daemon").success());
    }
}

#[test]
fn a_cli_read_cut_off_by_a_stop_waits_and_reaches_the_successor() {
    let fixture = fixture();
    let (startup, startup_path) = fixture.hook("startup.sock");
    let (stop_hook, stop_path) = fixture.hook("stop.sock");
    let (stopping, stopping_path) = fixture.hook("stopping.sock");
    let mut daemon = fixture.daemon(&[
        ("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path),
        ("TRELLIS_TEST_DAEMON_STOP_SOCKET", &stop_path),
        ("TRELLIS_TEST_DAEMON_STOPPING_SOCKET", &stopping_path),
    ]);
    // This status waits for the first index when the stop cuts it off.
    let mut status = fixture.spawn("status", fixture.status());
    let held = arrival(&startup, 1);
    let mut stop = stop_reaching_runtimes(&fixture, &stop_hook);
    // Its read-only retry meets the stopping daemon, which answers it typed.
    drop(arrival(&stopping, b'A'));
    release(held);
    assert!(stop.exit("daemon stop").success(), "{}", stop.stderr());
    assert!(daemon.exit("stopped daemon").success());
    // After the stop it re-runs the election and reads from the successor.
    assert!(status.exit("status").success(), "{}", status.stderr());
    assert!(
        status
            .stderr()
            .contains("the daemon is stopping; waiting to reach its successor"),
        "{}",
        status.stderr()
    );
}

#[test]
fn serve_during_a_stop_waits_and_registers_with_the_successor() {
    let fixture = fixture();
    let held = held_stop(&fixture);
    let mut serve = fixture.cli();
    serve
        .arg("serve")
        .arg("--workspace")
        .arg(&fixture.root)
        .args(["--bind", "127.0.0.1:0", "--token-file"])
        .arg(fixture.home.join("token"));
    let mut serve = fixture.spawn("serve", serve);
    held.answered();
    held.finish();
    trellis::testing::wait_until("serve registers with the successor", || {
        assert!(
            serve.child.try_wait().unwrap().is_none(),
            "serve exited: {}",
            serve.stderr()
        );
        serve.stderr().contains("Trellis: http://")
    });
    assert!(
        serve.stderr().contains("the daemon is stopping"),
        "{}",
        serve.stderr()
    );
}

/// `trellis mcp` with its stdin and stdout as pipes the test drives.
struct Relay {
    _proc: Proc,
    stdin: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
}

fn relay(fixture: &Fixture) -> Relay {
    let mut command = fixture.cli();
    command
        .arg("mcp")
        .arg("--workspace")
        .arg(&fixture.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(fs::File::create(fixture.home.join("mcp.err")).unwrap());
    let mut child = command.spawn().unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { return };
            if sender.send(line).is_err() {
                return;
            }
        }
    });
    Relay {
        _proc: Proc {
            child,
            err: fixture.home.join("mcp.err"),
            out: fixture.home.join("mcp.out"),
        },
        stdin,
        lines,
    }
}

impl Relay {
    fn send(&mut self, line: serde_json::Value) {
        writeln!(self.stdin, "{line}").unwrap();
        self.stdin.flush().unwrap();
    }
    /// The response with `id`, within the hang backstop.
    fn response(&self, id: u64) -> serde_json::Value {
        let deadline = Instant::now() + HANG_BACKSTOP;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self.lines.recv_timeout(left).expect("MCP response (hang)");
            let value: serde_json::Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }
    fn initialize(&mut self) {
        self.send(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}));
        assert!(self.response(1)["result"].is_object());
        self.send(serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    }
    fn describe(&mut self, id: u64) -> serde_json::Value {
        self.send(
            serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{
            "name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}}),
        );
        self.response(id)
    }
}

#[test]
fn an_mcp_launch_during_a_stop_waits_and_attaches_to_the_successor() {
    let fixture = fixture();
    let held = held_stop(&fixture);
    let mut relay = relay(&fixture);
    held.answered();
    held.finish();
    relay.initialize();
    let described = relay.describe(2);
    assert_eq!(
        described["result"]["structuredContent"]["schemaVersion"], 1,
        "{described}"
    );
    assert!(
        described["result"]["structuredContent"]["error"].is_null(),
        "{described}"
    );
}

#[test]
fn an_mcp_tool_call_during_a_stop_answers_the_retryable_daemon_stopping() {
    let fixture = fixture();
    let (startup, startup_path) = fixture.hook("startup.sock");
    let (stop_hook, stop_path) = fixture.hook("stop.sock");
    let mut daemon = fixture.daemon(&[
        ("TRELLIS_TEST_DAEMON_STARTUP_SOCKET", &startup_path),
        ("TRELLIS_TEST_DAEMON_STOP_SOCKET", &stop_path),
    ]);
    let mut relay = relay(&fixture);
    relay.initialize();
    let mut index = fixture.cli();
    index.arg("index").arg("--workspace").arg(&fixture.root);
    let mut index = fixture.spawn("index", index);
    let held = arrival(&startup, 1);
    let mut stop = stop_reaching_runtimes(&fixture, &stop_hook);
    assert!(!index.exit("index during the stop").success());
    // The session's reconnect meets the stopping daemon: a typed, retryable
    // answer within the call's own deadline, not a wait for the stop.
    let during = relay.describe(2);
    let error = &during["result"]["structuredContent"]["error"];
    assert_eq!(error["code"], "daemon_stopping", "{during}");
    assert_eq!(error["retryable"], true, "{during}");
    release(held);
    assert!(stop.exit("daemon stop").success(), "{}", stop.stderr());
    assert!(daemon.exit("stopped daemon").success());
    // The next call reaches the successor.
    let after = relay.describe(3);
    assert!(
        after["result"]["structuredContent"]["error"].is_null(),
        "{after}"
    );
}

#[test]
fn an_mcp_attach_that_times_out_is_typed_not_a_raw_os_error() {
    let fixture = fixture();
    // A listener that accepts into its backlog and never answers.
    let socket = fixture.socket();
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let _silent = UnixListener::bind(&socket).unwrap();
    let mut command = fixture.cli();
    command.arg("mcp").arg("--workspace").arg(&fixture.root);
    let mut mcp = fixture.spawn("mcp", command);
    assert!(!mcp.exit("mcp against a silent daemon").success());
    let stderr = mcp.stderr();
    assert!(stderr.contains("daemon_unavailable"), "{stderr}");
    assert!(!stderr.contains("os error"), "{stderr}");
}

#[test]
fn a_bare_daemon_that_loses_to_a_stopping_owner_says_so() {
    let fixture = fixture();
    let held = held_stop(&fixture);
    let mut bare = fixture.cli();
    bare.arg("daemon");
    let mut bare = fixture.spawn("bare-daemon", bare);
    // Losing the election is not a failure, but it is never silent.
    assert!(bare.exit("losing daemon").success());
    assert!(bare.stderr().contains("is stopping"), "{}", bare.stderr());
    held.answered();
    held.finish();
    // Against a running owner it says that instead.
    let _owner = fixture.daemon(&[]);
    let mut bare = fixture.cli();
    bare.arg("daemon");
    let mut bare = fixture.spawn("bare-daemon-2", bare);
    assert!(bare.exit("losing daemon").success());
    assert!(
        bare.stderr().contains("already running"),
        "{}",
        bare.stderr()
    );
}

/// A stand-in for a stopping daemon at the fixture's socket: it answers its first
/// connection `daemon_stopping` after `first`, and every later one after `later`, or
/// never (`None`, holding the connection open), as an attempt that would outlive its
/// wait meets.
struct StoppingDaemon {
    _socket: std::path::PathBuf,
}

impl StoppingDaemon {
    fn bind(socket: &std::path::Path, first: Duration, later: Option<Duration>) -> Self {
        use trellis::daemon::protocol;
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            let deadline = Instant::now() + HANG_BACKSTOP;
            let mut index = 0;
            while Instant::now() < deadline {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return,
                };
                stream.set_nonblocking(false).unwrap();
                let delay = if index == 0 { Some(first) } else { later };
                index += 1;
                let Some(delay) = delay else {
                    held.push(stream);
                    continue;
                };
                std::thread::spawn(move || {
                    let Ok(request) = protocol::read_frame::<protocol::Request>(&mut stream) else {
                        return;
                    };
                    std::thread::sleep(delay);
                    let _ = protocol::write_frame(
                        &mut stream,
                        &protocol::Reply {
                            id: request.id,
                            payload: serde_json::json!({"error": protocol::DAEMON_STOPPING_MESSAGE}),
                        },
                    );
                });
            }
        });
        Self {
            _socket: socket.to_path_buf(),
        }
    }
}

/// Wall time a bounded wait may take beyond its bound: process start and the last pause,
/// with room for a loaded runner. The 12 s ceiling still separates the bounded case
/// (about 2 s) from an unbounded one (27.5 s and 60.8 s at the parent).
const BOUND_MARGIN: Duration = Duration::from_secs(10);

/// The stop wait's one deadline holds even when an attempt inside it would outlive it:
/// the silent second attempt gets only what is left of the wait, and the command fails
/// typed `daemon_stopping` at the bound.
#[test]
fn a_cli_stop_wait_ends_at_its_bound_when_an_attempt_would_outlive_it() {
    let fixture = fixture();
    let _daemon = StoppingDaemon::bind(&fixture.socket(), Duration::ZERO, None);
    let bound = Duration::from_secs(2);
    let mut status = fixture.status();
    status.env(
        "TRELLIS_TEST_CLI_STOP_WAIT_MS",
        bound.as_millis().to_string(),
    );
    let started = Instant::now();
    let mut status = fixture.spawn("status", status);
    assert!(!status.exit("status during a stop").success());
    let elapsed = started.elapsed();
    assert!(
        elapsed < bound + BOUND_MARGIN,
        "the wait took {elapsed:?}, past its {bound:?} bound"
    );
    assert!(
        status.stderr().contains("daemon_stopping"),
        "{}",
        status.stderr()
    );
}

/// `trellis api` keeps its own reply bound for the whole call, a stop wait included: answers
/// that each arrive within one read bound cannot stretch the call past it.
#[test]
fn trellis_api_keeps_its_own_bound_through_a_stop_wait() {
    let fixture = fixture();
    let answer = Duration::from_millis(1500);
    let _daemon = StoppingDaemon::bind(&fixture.socket(), answer, Some(answer));
    let bound = Duration::from_secs(2);
    let mut api = fixture.cli();
    api.args(["api", "GET", "/api/checkouts"]).env(
        "TRELLIS_TEST_CLI_REPLY_BOUND_MS",
        bound.as_millis().to_string(),
    );
    let started = Instant::now();
    let mut api = fixture.spawn("api", api);
    assert!(!api.exit("api during a stop").success());
    let elapsed = started.elapsed();
    assert!(
        elapsed < bound + BOUND_MARGIN,
        "the call took {elapsed:?}, past its {bound:?} bound"
    );
    let output = api.stdout() + &api.stderr();
    assert!(output.contains("daemon_stopping"), "{output}");
}

/// A stand-in that answers its first connection `daemon_stopping`, then, as a successor
/// would, streams progress frames on the next one for `progress` before a successful
/// `trellis api` answer.
fn progressing_successor(socket: &std::path::Path, progress: Duration) {
    use trellis::daemon::protocol;
    fs::create_dir_all(socket.parent().unwrap()).unwrap();
    let listener = UnixListener::bind(socket).unwrap();
    std::thread::spawn(move || {
        for index in 0..2 {
            let mut stream = trellis::testing::accept_within_backstop(&listener);
            let Ok(request) = protocol::read_frame::<protocol::Request>(&mut stream) else {
                return;
            };
            let reply = |payload| protocol::Reply {
                id: request.id,
                payload,
            };
            if index == 0 {
                let error = serde_json::json!({"error": protocol::DAEMON_STOPPING_MESSAGE});
                let _ = protocol::write_frame(&mut stream, &reply(error));
                continue;
            }
            let end = Instant::now() + progress;
            while Instant::now() < end {
                let frame = serde_json::json!({"progress": "trellis: indexing this checkout"});
                if protocol::write_frame(&mut stream, &reply(frame)).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            let done = serde_json::json!({"result": {"status": 200, "body": {}}});
            let _ = protocol::write_frame(&mut stream, &reply(done));
        }
    });
}

/// The stop wait's deadline holds across every frame of the attempt after it: progress
/// frames that each arrive within one read bound cannot carry the call past it, and a
/// success that completes after it is refused (#1293 review).
#[test]
fn a_late_success_after_a_stop_wait_is_refused() {
    let fixture = fixture();
    progressing_successor(&fixture.socket(), Duration::from_secs(3));
    let mut api = fixture.cli();
    api.args(["api", "GET", "/api/checkouts"])
        .env("TRELLIS_TEST_CLI_REPLY_BOUND_MS", "1000");
    let mut api = fixture.spawn("api", api);
    assert!(
        !api.exit("api after a stop").success(),
        "a success after the bound was accepted: {}",
        api.stdout()
    );
    let output = api.stdout() + &api.stderr();
    assert!(output.contains("daemon_stopping"), "{output}");
}

/// The `trellis mcp` launch attach draws its connect and its attach exchange from the
/// stop wait's one deadline. Here the connect takes most of the wait (the socket is gone
/// and a candidate cannot win the election this test holds), and the attach answer comes
/// after the wait's deadline but within a fresh attach budget: it must not be accepted.
#[test]
fn an_mcp_launch_attach_spends_one_deadline_across_connect_and_attach() {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    use trellis::daemon::protocol;
    let fixture = fixture();
    let paths = trellis::daemon::SocketPaths::new(&fixture.roots().data);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&paths.run)
        .unwrap();
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .open(&paths.lock)
        .unwrap();
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let socket = paths.socket.clone();
    let first = UnixListener::bind(&socket).unwrap();
    // The wait is 4 s; the socket returns at 2.4 s and the attach answer at 5.2 s.
    let (rebind, answer) = (Duration::from_millis(2400), Duration::from_millis(5200));
    let stand_in = std::thread::spawn(move || {
        let mut stream = trellis::testing::accept_within_backstop(&first);
        let request: protocol::Request = protocol::read_frame(&mut stream).unwrap();
        let started = Instant::now();
        protocol::write_frame(
            &mut stream,
            &protocol::Reply {
                id: request.id,
                payload: serde_json::json!({"error": protocol::DAEMON_STOPPING_MESSAGE}),
            },
        )
        .unwrap();
        drop(first);
        fs::remove_file(&socket).unwrap();
        std::thread::sleep(rebind.saturating_sub(started.elapsed()));
        let second = UnixListener::bind(&socket).unwrap();
        let mut stream = trellis::testing::accept_within_backstop(&second);
        let Ok(request) = protocol::read_frame::<protocol::Request>(&mut stream) else {
            return;
        };
        std::thread::sleep(answer.saturating_sub(started.elapsed()));
        let _ = protocol::write_frame(
            &mut stream,
            &protocol::Reply {
                id: request.id,
                payload: serde_json::json!({"result": true, "mcpIdentity": protocol::mcp_identity()}),
            },
        );
        // Hold the connection until the relay is done with it.
        let _ = protocol::read_frame::<protocol::Request>(&mut stream);
    });
    let mut mcp = fixture.cli();
    mcp.arg("mcp")
        .arg("--workspace")
        .arg(&fixture.root)
        .env("TRELLIS_TEST_MCP_STOP_WAIT_MS", "4000");
    // With no input, an attached relay exits 0 at once; only a refused attach fails.
    let mut mcp = fixture.spawn("mcp", mcp);
    assert!(
        !mcp.exit("mcp launch during a stop").success(),
        "an attach answered after the wait's deadline was accepted"
    );
    assert!(
        mcp.stderr().contains("daemon_unavailable"),
        "{}",
        mcp.stderr()
    );
    drop(lock);
    let _ = stand_in.join();
}
