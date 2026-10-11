//! Reconnection policy: repeat only read-only calls after uncertain delivery.
use super::protocol::{self, Reply, Request};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    DaemonUnavailable,
    OutcomeUnknown,
}
impl CallError {
    pub fn code(self) -> &'static str {
        match self {
            Self::DaemonUnavailable => "daemon_unavailable",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}

/// The starter reports election loss separately from unexpected startup failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartOutcome {
    Started,
    ElectionInProgress,
}

/// Start is invoked only after a failed initial connect. Concurrent starters may both
/// invoke it; the daemon's flock elects exactly one listener.
pub fn connect_or_start(
    socket: &Path,
    mut start: impl FnMut() -> io::Result<StartOutcome>,
    timeout: Duration,
) -> io::Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
    let _ = start()?; // An election loser waits for the winner within the same deadline.
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(stream) = UnixStream::connect(socket) {
            return Ok(stream);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon unavailable",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn call(
    request: &Request,
    read_only: bool,
    mut connect: impl FnMut() -> io::Result<UnixStream>,
) -> Result<Reply, CallError> {
    let attempts = if read_only { 2 } else { 1 };
    for attempt in 0..attempts {
        let mut stream = match connect() {
            Ok(stream) => stream,
            Err(_) if attempt + 1 < attempts => continue,
            Err(_) => return Err(CallError::DaemonUnavailable),
        };
        // A partially written request can still have reached the server. Never replay
        // a mutation after attempting its write, including when the reply is lost.
        let reply = protocol::write_frame(&mut stream, request)
            .and_then(|()| read_reply(&mut stream, request.id));
        match reply {
            Ok(reply) => return Ok(reply),
            Err(_) if read_only && attempt + 1 < attempts => continue,
            Err(_) if read_only => return Err(CallError::DaemonUnavailable),
            Err(_) => return Err(CallError::OutcomeUnknown),
        }
    }
    Err(CallError::DaemonUnavailable)
}

/// One large CLI export is a sequence of individually bounded frames. The
/// final JSON is parsed only after every frame with the same request ID arrives.
fn read_reply(stream: &mut UnixStream, id: u64) -> io::Result<Reply> {
    let mut reply: Reply = protocol::read_frame(stream)?;
    if reply.id != id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon reply ID mismatch",
        ));
    }
    if reply.payload.get("chunk").is_none() {
        return Ok(reply);
    }
    let mut bytes = Vec::new();
    loop {
        let chunk = reply
            .payload
            .get("chunk")
            .and_then(|value| value.as_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid daemon chunk"))?;
        let part = hex::decode(chunk).map_err(io::Error::other)?;
        bytes.extend_from_slice(&part);
        let more = reply
            .payload
            .get("more")
            .and_then(|value| value.as_bool())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "missing daemon chunk flag")
            })?;
        if !more {
            return Ok(Reply {
                id,
                payload: serde_json::from_slice(&bytes).map_err(io::Error::other)?,
            });
        }
        reply = protocol::read_frame(stream)?;
        if reply.id != id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "daemon reply ID mismatch",
            ));
        }
    }
}

/// Attach the original verified launch identity to every daemon generation.
fn attach_mcp(
    mut stream: UnixStream,
    workspace: &crate::store::topology::WorkspaceIdentity,
) -> io::Result<UnixStream> {
    let request = Request {
        id: 1,
        operation: "mcp".into(),
        payload: serde_json::json!({
            "workspace": workspace.root,
            "device": workspace.device,
            "inode": workspace.inode,
        }),
    };
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    protocol::write_frame(&mut stream, &request)?;
    let reply: Reply = protocol::read_frame(&mut stream)?;
    if reply.id != request.id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon reply ID mismatch",
        ));
    }
    if let Some(error) = reply.payload.get("error").and_then(|v| v.as_str()) {
        return Err(io::Error::other(error.to_owned()));
    }
    if reply.payload.get("result") != Some(&serde_json::Value::Bool(true)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid MCP attach reply",
        ));
    }
    stream.set_read_timeout(Some(Duration::from_millis(50)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

fn mcp_failure(
    value: &serde_json::Value,
    code: &str,
    workspace: &crate::store::topology::WorkspaceIdentity,
) -> serde_json::Value {
    use serde_json::json;
    let id = value.get("id").cloned().unwrap_or_default();
    if matches!(code, "daemon_unavailable" | "selection_unavailable")
        && value.get("method").and_then(|v| v.as_str()) == Some("tools/call")
        && value
            .pointer("/params/name")
            .and_then(|v| v.as_str())
            .is_some_and(crate::mcp::catalog::known)
    {
        // An interrupted attachment has not verified any answering checkout.
        // Validate application fields before classifying an attempted selection.
        let name = value
            .pointer("/params/name")
            .and_then(|v| v.as_str())
            .unwrap();
        let arguments = value.pointer("/params/arguments");
        let selected = arguments
            .and_then(serde_json::Value::as_object)
            .and_then(|object| object.get("workspace"));
        let envelope = if let Err(error) = crate::mcp::tools::validate(name, arguments) {
            let mut failure = crate::mcp::tools::failure(&id, error);
            if let Some(selected) = selected
                && let Some(object) = arguments.and_then(serde_json::Value::as_object)
            {
                let mut without = object.clone();
                without.remove("workspace");
                if crate::mcp::tools::validate(name, Some(&serde_json::Value::Object(without)))
                    .is_ok()
                    && selected.as_str().is_none_or(|path| path.len() > 4096)
                {
                    failure["error"]["attemptedWorkspace"] = crate::mcp::tools::attempted(selected);
                }
            }
            failure
        } else {
            let attempted = selected.cloned().unwrap_or_else(|| json!(workspace.root));
            let reason = if selected
                .and_then(serde_json::Value::as_str)
                .is_some_and(|path| !std::path::Path::new(path).is_absolute())
            {
                crate::daemon::registry::SelectionError::NotAbsolute
            } else {
                crate::daemon::registry::SelectionError::Unavailable
            };
            crate::mcp::tools::selection_failure(&id, reason, &attempted)
        };
        let modern = value.pointer("/params/_meta").is_some();
        return json!({"jsonrpc":"2.0","id":id,
            "result":crate::mcp::tools::result(envelope, modern)});
    }
    json!({"jsonrpc":"2.0","id":id,
        "error":{"code":-32603,"message":"Internal error","data":{"code":code}}})
}

/// Apply the daemon's own pure admission reducer to protocol decisions that
/// cannot be delegated to a disconnected or saturated daemon generation.
fn protocol_event(
    request: crate::mcp::wire::Request,
    mode: crate::mcp::session::Mode,
) -> (crate::mcp::session::Event, crate::mcp::session::Mode) {
    use crate::mcp::{catalog, session, wire};
    use serde_json::json;
    let mut session = session::Session::new();
    let bootstrap = match mode {
        session::Mode::Unselected => None,
        session::Mode::Modern => Some((
            "server/discover",
            json!({"_meta":{
                "io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}
            }}),
        )),
        session::Mode::LegacyAwaitInitialized | session::Mode::LegacyReady => Some((
            "initialize",
            json!({"protocolVersion":"2025-11-25",
                "capabilities":{},"clientInfo":{"name":"relay","version":"1"}}),
        )),
    };
    if let Some((method, params)) = bootstrap {
        let seed = wire::Request {
            id: Some(json!("relay-bootstrap")),
            key: Some(wire::IdKey::String("relay-bootstrap".into())),
            method: method.into(),
            params: Some(params),
        };
        let session::Event::Admitted(admission) = session.accept(seed, catalog::known) else {
            unreachable!("fixed relay bootstrap must be valid")
        };
        let prepared = session
            .prepare(admission.token, json!(null))
            .expect("bootstrap pending");
        session.commit(prepared).expect("bootstrap committed");
        if mode == session::Mode::LegacyReady {
            let notification = wire::Request {
                id: None,
                key: None,
                method: "notifications/initialized".into(),
                params: None,
            };
            let _ = session.accept(notification, catalog::known);
        }
    }
    let event = session.accept(request, catalog::known);
    (event, session.mode())
}

struct McpInputBuffer<'a> {
    workspace: &'a crate::store::topology::WorkspaceIdentity,
    queue: std::collections::VecDeque<crate::mcp::wire::Frame>,
    cancelled_queued: Vec<serde_json::Value>,
    mode: crate::mcp::session::Mode,
    active_id: Option<serde_json::Value>,
    active_key: Option<crate::mcp::wire::IdKey>,
    cancelled_active: bool,
    stdin: std::sync::Arc<StdinFence>,
    /// A frame taken from the channel but not yet handled; it precedes every
    /// frame still in the channel.
    deferred: Option<(crate::mcp::wire::Frame, u64)>,
    /// While set, `drain_mcp_input` handles only frames whose bytes end at or
    /// before this stdin offset and defers the first later one.
    drain_through: Option<u64>,
}

/// Frames from the stdin reader, each with the stdin offset where it ends.
type FrameReceiver = std::sync::mpsc::Receiver<(crate::mcp::wire::Frame, u64)>;

/// Lets the relay prove that every byte the client wrote before a decision
/// point has reached the frame channel (#127). Each read and its byte count
/// happen under `io`, so `read_total + FIONREAD` sampled under the same lock
/// counts exactly the bytes written so far. Each time the reader asks for more
/// input, which `BufReader` does only after every complete frame before that
/// offset has been sent, it publishes the offset as `ready_through`.
struct StdinFence {
    fd: std::os::fd::RawFd,
    io: std::sync::Mutex<()>,
    read_total: std::sync::atomic::AtomicU64,
    ready_through: std::sync::atomic::AtomicU64,
    closed: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    fail_fionread: std::sync::atomic::AtomicBool,
    /// Runs once, immediately after the decision offset is sampled.
    #[cfg(test)]
    after_sample: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl StdinFence {
    fn new(fd: std::os::fd::RawFd) -> Self {
        Self {
            fd,
            io: std::sync::Mutex::new(()),
            read_total: Default::default(),
            ready_through: Default::default(),
            closed: Default::default(),
            #[cfg(test)]
            fail_fionread: Default::default(),
            #[cfg(test)]
            after_sample: Default::default(),
        }
    }

    /// Bytes the client had written at this instant, or `None` when pending
    /// input cannot be measured and the decision therefore cannot be fenced.
    fn written(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::SeqCst;
        let sampled = {
            let _io = self.io.lock().unwrap();
            let mut pending: libc::c_int = 0;
            #[cfg(test)]
            let failed = self.fail_fionread.load(SeqCst);
            #[cfg(not(test))]
            let failed = false;
            (!failed && unsafe { libc::ioctl(self.fd, libc::FIONREAD, &mut pending) } == 0)
                .then(|| self.read_total.load(SeqCst) + pending.max(0) as u64)
        };
        #[cfg(test)]
        {
            // Release the hook's lock first: a hook may sample again.
            let hook = self.after_sample.lock().unwrap().take();
            if let Some(hook) = hook {
                hook();
            }
        }
        sampled
    }

    /// Every complete frame within the first `offset` bytes has been sent.
    fn settled(&self, offset: u64) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.closed.load(SeqCst) || self.ready_through.load(SeqCst) >= offset
    }
}

/// Unbuffered reads of one descriptor that publish how far input is settled.
struct FencedReader {
    fd: std::os::fd::RawFd,
    fence: std::sync::Arc<StdinFence>,
}

impl io::Read for FencedReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        use std::sync::atomic::Ordering::SeqCst;
        self.fence
            .ready_through
            .store(self.fence.read_total.load(SeqCst), SeqCst);
        let mut ready = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let rc = unsafe { libc::poll(&mut ready, 1, -1) };
            if rc >= 0 {
                break;
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
        let _io = self.fence.io.lock().unwrap();
        loop {
            let n = unsafe { libc::read(self.fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                self.fence.read_total.fetch_add(n as u64, SeqCst);
                return Ok(n as usize);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// Spawn the bounded frame reader for one descriptor. Each frame carries the
/// stdin offset just past its last byte.
fn spawn_frame_reader(
    fd: std::os::fd::RawFd,
) -> io::Result<(FrameReceiver, std::sync::Arc<StdinFence>)> {
    use crate::mcp::wire::{self, Frame};
    use std::sync::atomic::Ordering::SeqCst;
    let fence = std::sync::Arc::new(StdinFence::new(fd));
    let (sender, receiver) = std::sync::mpsc::sync_channel::<(Frame, u64)>(16);
    let reader_fence = fence.clone();
    thread::Builder::new()
        .name("mcp-stdin-relay".into())
        .spawn(move || {
            let mut reader = io::BufReader::new(FencedReader {
                fd,
                fence: reader_fence.clone(),
            });
            loop {
                let frame = match wire::read_frame(&mut reader) {
                    Ok(frame) => frame,
                    Err(error) => {
                        use std::io::Write;
                        let _ = writeln!(io::stderr(), "MCP stdin read failed: {error}");
                        Frame::Eof
                    }
                };
                let end = reader_fence.read_total.load(SeqCst) - reader.buffer().len() as u64;
                let last = matches!(frame, Frame::Eof);
                if sender.send((frame, end)).is_err() || last {
                    break;
                }
            }
            reader_fence.closed.store(true, SeqCst);
        })?;
    Ok((receiver, fence))
}

/// A failed attach has no socket to receive cancellation. Consume ready stdin first.
fn discard_failed_mcp_result(
    receiver: &FrameReceiver,
    buffer: &mut McpInputBuffer<'_>,
    stdout: &mut impl std::io::Write,
    eof: &mut bool,
    active_id: Option<&serde_json::Value>,
) -> io::Result<bool> {
    let mut canceled = false;
    drain_mcp_input(
        None,
        receiver,
        buffer,
        stdout,
        eof,
        active_id,
        &mut canceled,
    )?;
    Ok(*eof || canceled || buffer.cancelled_active)
}

/// What the relay may do with a complete daemon reply.
enum ReplyGate {
    Write,
    Cancelled,
    /// Input before the decision point could not be settled in time.
    Unknown,
}

/// The decision point for a daemon reply (#127, owner ruling in
/// docs/mcp-readonly-pilot-contract.md). Only frames whose bytes the client had
/// completely written when the decision offset is sampled can affect it: a
/// cancel among them wins even if the stdin reader has not delivered it yet.
/// The first later frame is deferred unhandled until after the reply, so a
/// cancel written after the decision is concurrent and cannot suppress it, and
/// later input cannot extend the wait (T00).
fn reply_gate(
    stream: &mut UnixStream,
    receiver: &FrameReceiver,
    buffer: &mut McpInputBuffer<'_>,
    stdout: &mut impl std::io::Write,
    eof: &mut bool,
    active_id: Option<&serde_json::Value>,
) -> io::Result<ReplyGate> {
    let Some(written) = buffer.stdin.written() else {
        return Ok(ReplyGate::Unknown);
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut canceled = false;
    loop {
        // Sampled first: frames within `written` sent before settling are then
        // all in the channel for the drain below.
        let settled = buffer.stdin.settled(written);
        buffer.drain_through = Some(written);
        let drained = drain_mcp_input(
            Some(&mut *stream),
            receiver,
            buffer,
            stdout,
            eof,
            active_id,
            &mut canceled,
        );
        buffer.drain_through = None;
        drained?;
        if *eof || canceled || buffer.cancelled_active {
            return Ok(ReplyGate::Cancelled);
        }
        // A deferred later frame proves every earlier frame was handled.
        if settled || buffer.deferred.is_some() {
            return Ok(ReplyGate::Write);
        }
        if Instant::now() >= deadline {
            return Ok(ReplyGate::Unknown);
        }
        thread::sleep(Duration::from_millis(1));
    }
}

/// One bounded stdin reader and one response writer survive daemon generations.
/// Only an interrupted read can be replayed; unknown methods are never retried.
pub fn relay_stdio(
    stream: UnixStream,
    workspace: &crate::store::topology::WorkspaceIdentity,
    mut reconnect: impl FnMut() -> io::Result<UnixStream>,
) -> io::Result<()> {
    use crate::mcp::wire::{self, Frame};
    use std::io::Write;
    let mut current = Some(attach_mcp(stream, workspace)?);
    let (receiver, stdin_fence) = spawn_frame_reader(0)?;
    let mut buffer = McpInputBuffer {
        workspace,
        queue: std::collections::VecDeque::new(),
        cancelled_queued: Vec::new(),
        mode: crate::mcp::session::Mode::Unselected,
        active_id: None,
        active_key: None,
        cancelled_active: false,
        stdin: stdin_fence,
        deferred: None,
        drain_through: None,
    };
    let mut receive_bytes = Vec::new();
    let mut stdout = io::stdout().lock();
    let mut legacy_init: Option<Vec<u8>> = None;
    let mut legacy_ready = false;
    let mut eof = false;
    loop {
        let frame = if let Some(frame) = buffer.queue.pop_front() {
            frame
        } else if let Some((frame, _)) = buffer.deferred.take() {
            frame
        } else if eof {
            break;
        } else {
            receiver.recv().map_or(Frame::Eof, |(frame, _)| frame)
        };
        let line = match frame {
            Frame::Eof => break,
            Frame::Oversized => {
                wire::write_response(
                    &mut stdout,
                    &wire::ProtocolError::new(-32600, serde_json::Value::Null).response(),
                )?;
                continue;
            }
            Frame::Line(line) => line,
        };
        let admitted = match wire::decode(Frame::Line(line.clone())) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(error) => {
                wire::write_response(&mut stdout, &error.response())?;
                continue;
            }
        };
        let mut value: serde_json::Value = serde_json::from_slice(&line).expect("decoded MCP line");
        if let Some(id) = admitted.id.clone() {
            value["id"] = id;
        }
        let request_key = admitted.key.clone();
        let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
        let has_id = value.get("id").is_some();
        if has_id
            && let Some(at) = buffer
                .cancelled_queued
                .iter()
                .position(|id| Some(id) == value.get("id"))
        {
            buffer.cancelled_queued.remove(at);
            continue;
        }
        let (event, mode) = protocol_event(admitted, buffer.mode);
        buffer.mode = mode;
        match event {
            crate::mcp::session::Event::Error(error) => {
                wire::write_response(&mut stdout, &error.response())?;
                continue;
            }
            crate::mcp::session::Event::Admitted(admission)
                if matches!(admission.action, crate::mcp::session::Action::Unknown) =>
            {
                wire::write_response(
                    &mut stdout,
                    &wire::ProtocolError::new(-32601, admission.id).response(),
                )?;
                continue;
            }
            _ => (),
        }
        let read_only = matches!(
            method,
            "server/discover" | "tools/list" | "tools/call" | "initialize"
        );
        if method == "initialize" && has_id {
            legacy_init = Some(line.clone());
            legacy_ready = false;
        }
        if method == "notifications/initialized" && mode == crate::mcp::session::Mode::LegacyReady {
            legacy_ready = true;
        }
        buffer.active_id = value.get("id").cloned();
        buffer.active_key = request_key;
        buffer.cancelled_active = false;
        let mut sent = line.clone();
        sent.push(b'\n');
        let mut retry = 0;
        loop {
            if current.is_none() {
                receive_bytes.clear();
                current = reconnect()
                    .and_then(|stream| attach_mcp(stream, workspace))
                    .ok();
                if let (Some(stream), true, Some(init)) =
                    (current.as_mut(), legacy_ready, legacy_init.as_ref())
                    && method != "initialize"
                {
                    let mut setup = init.clone();
                    setup.push(b'\n');
                    let init_id = serde_json::from_slice::<serde_json::Value>(init)
                        .ok()
                        .and_then(|value| value.get("id").cloned())
                        .unwrap_or_default();
                    let setup_result = stream
                        .write_all(&setup)
                        .and_then(|()| {
                            read_mcp_line(
                                stream,
                                &receiver,
                                &mut buffer,
                                &mut stdout,
                                &mut eof,
                                None, // synthetic setup has no cancellable external ID
                                &mut receive_bytes,
                            )
                            .and_then(|reply| {
                                reply.ok_or_else(|| io::Error::other("MCP setup canceled"))
                            })
                            .and_then(|reply| {
                                if reply.get("id") == Some(&init_id) {
                                    Ok(())
                                } else {
                                    Err(io::Error::new(
                                        io::ErrorKind::InvalidData,
                                        "MCP setup reply ID mismatch",
                                    ))
                                }
                            })
                        })
                        .and_then(|()| {
                            stream.write_all(
                                b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
                            )
                        });
                    if setup_result.is_err() {
                        current = None;
                    }
                }
            }
            if eof {
                break;
            }
            let Some(stream) = current.as_mut() else {
                let discard = discard_failed_mcp_result(
                    &receiver,
                    &mut buffer,
                    &mut stdout,
                    &mut eof,
                    value.get("id"),
                )?;
                if has_id && !discard {
                    wire::write_response(
                        &mut stdout,
                        &mcp_failure(&value, "daemon_unavailable", workspace),
                    )?;
                }
                break;
            };
            let mut canceled_before_send = false;
            drain_mcp_input(
                Some(stream),
                &receiver,
                &mut buffer,
                &mut stdout,
                &mut eof,
                value.get("id"),
                &mut canceled_before_send,
            )?;
            if eof || canceled_before_send || buffer.cancelled_active {
                break;
            }
            if stream.write_all(&sent).is_err() {
                current = None;
            } else if !has_id {
                break;
            } else {
                match read_mcp_line(
                    stream,
                    &receiver,
                    &mut buffer,
                    &mut stdout,
                    &mut eof,
                    value.get("id"),
                    &mut receive_bytes,
                ) {
                    Ok(Some(reply)) => {
                        if reply.get("id") != value.get("id") {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "daemon MCP reply ID mismatch",
                            ));
                        }
                        crate::mcp::phase_hook("relay_before_write")?;
                        // The whole reply was read, so a dropped reply leaves the
                        // stream at a frame boundary.
                        match reply_gate(
                            stream,
                            &receiver,
                            &mut buffer,
                            &mut stdout,
                            &mut eof,
                            value.get("id"),
                        )? {
                            ReplyGate::Write => {
                                crate::mcp::phase_hook("relay_after_decision")?;
                                wire::write_response(&mut stdout, &reply)?;
                            }
                            ReplyGate::Cancelled => {}
                            ReplyGate::Unknown => wire::write_response(
                                &mut stdout,
                                &mcp_failure(&value, "outcome_unknown", workspace),
                            )?,
                        }
                        break;
                    }
                    Ok(None) => {
                        if !eof {
                            // No response from the canceled generation can reach a later ID.
                            let _ = stream.shutdown(std::net::Shutdown::Both);
                            current = None;
                            receive_bytes.clear();
                        }
                        break;
                    }
                    Err(error) => {
                        let partial = error.kind() == io::ErrorKind::InvalidData;
                        current = None;
                        receive_bytes.clear();
                        if eof {
                            break;
                        }
                        // Never replay a possibly delivered mutation or a truncated reply.
                        if partial || !read_only || retry != 0 {
                            let discard = discard_failed_mcp_result(
                                &receiver,
                                &mut buffer,
                                &mut stdout,
                                &mut eof,
                                value.get("id"),
                            )?;
                            if !discard {
                                wire::write_response(
                                    &mut stdout,
                                    &mcp_failure(
                                        &value,
                                        if read_only {
                                            "daemon_unavailable"
                                        } else {
                                            "outcome_unknown"
                                        },
                                        workspace,
                                    ),
                                )?;
                            }
                            break;
                        }
                        retry += 1;
                        continue;
                    }
                }
            }
            if !read_only || retry != 0 {
                let discard = discard_failed_mcp_result(
                    &receiver,
                    &mut buffer,
                    &mut stdout,
                    &mut eof,
                    value.get("id"),
                )?;
                if has_id && !discard {
                    wire::write_response(
                        &mut stdout,
                        &mcp_failure(
                            &value,
                            if read_only {
                                "daemon_unavailable"
                            } else {
                                "outcome_unknown"
                            },
                            workspace,
                        ),
                    )?;
                }
                break;
            }
            retry += 1;
        }
        buffer.active_id = None;
        buffer.active_key = None;
        buffer.cancelled_active = false;
        if eof {
            break;
        }
    }
    if let Some(stream) = current {
        let _ = stream.shutdown(std::net::Shutdown::Write);
    }
    Ok(())
}

fn cancellation_id(frame: &crate::mcp::wire::Frame) -> Option<serde_json::Value> {
    use crate::mcp::wire::{self, Frame};
    let Frame::Line(line) = frame else {
        return None;
    };
    let request = wire::decode(Frame::Line(line.clone())).ok()??;
    if request.id.is_some() || request.method != "notifications/cancelled" {
        return None;
    }
    let params = request.params?.as_object()?.clone();
    if !params
        .keys()
        .all(|key| key == "requestId" || key == "reason")
        || params.get("reason").is_some_and(|value| !value.is_string())
    {
        return None;
    }
    match params.get("requestId")? {
        serde_json::Value::String(text) if wire::compact_string_token_len(text) <= 256 => {
            Some(serde_json::Value::String(text.clone()))
        }
        serde_json::Value::Number(number) => {
            wire::exact_safe_integer(&number.to_string()).map(serde_json::Value::from)
        }
        _ => None,
    }
}

/// Check all ready input before committing a reply. Once the buffer.queue is full,
/// reject excess admissions explicitly; never let them hide a later cancellation.
fn pending_duplicate(buffer: &McpInputBuffer<'_>, key: &crate::mcp::wire::IdKey) -> bool {
    use crate::mcp::wire::{self, Frame};
    (!buffer.cancelled_active && buffer.active_key.as_ref() == Some(key))
        || buffer.queue.iter().any(|queued| {
            matches!(queued,
            Frame::Line(line) if wire::decode(Frame::Line(line.clone()))
                .ok().flatten().is_some_and(|item|
                    item.key.as_ref() == Some(key)
                    && item.id.as_ref().is_some_and(|id|
                        !buffer.cancelled_queued.contains(id))))
        })
}

fn drain_mcp_input(
    mut stream: Option<&mut UnixStream>,
    receiver: &FrameReceiver,
    buffer: &mut McpInputBuffer<'_>,
    stdout: &mut impl std::io::Write,
    eof: &mut bool,
    active_id: Option<&serde_json::Value>,
    canceled: &mut bool,
) -> io::Result<()> {
    use crate::mcp::wire::{self, Frame};
    use std::io::Write;
    use std::sync::mpsc::TryRecvError;
    loop {
        let next = match buffer.deferred.take() {
            Some(entry) => Ok(entry),
            None => receiver.try_recv(),
        };
        let next = match next {
            Ok((frame, end)) if buffer.drain_through.is_some_and(|through| end > through) => {
                buffer.deferred = Some((frame, end));
                break;
            }
            other => other.map(|(frame, _)| frame),
        };
        match next {
            Ok(Frame::Eof) => {
                *eof = true;
                return Ok(());
            }
            Ok(frame) => {
                if let Some(id) = cancellation_id(&frame) {
                    if active_id == Some(&id) {
                        *canceled = true;
                        buffer.cancelled_active = true;
                    } else if buffer.active_id.as_ref() == Some(&id) {
                        // The external call may be popped while synthetic setup runs.
                        buffer.cancelled_active = true;
                    } else if buffer.queue.iter().any(|frame| {
                        matches!(frame,
                        Frame::Line(line) if wire::decode(Frame::Line(line.clone())).ok()
                            .flatten().and_then(|request| request.id).as_ref() == Some(&id))
                    }) && !buffer.cancelled_queued.contains(&id)
                    {
                        buffer.cancelled_queued.push(id);
                    }
                    if let Frame::Line(mut line) = frame {
                        line.push(b'\n');
                        // Cancellation takes effect locally even if the old socket died.
                        if let Some(stream) = stream.as_mut() {
                            let _ = stream.write_all(&line);
                        }
                    }
                } else if buffer.queue.len() < 16 {
                    let duplicate = match &frame {
                        Frame::Line(line) => wire::decode(Frame::Line(line.clone()))
                            .ok()
                            .flatten()
                            .and_then(|request| request.key.map(|key| (key, request.id)))
                            .filter(|(key, _)| pending_duplicate(buffer, key)),
                        _ => None,
                    };
                    if let Some((_, Some(id))) = duplicate {
                        wire::write_response(
                            stdout,
                            &wire::ProtocolError::new(-32600, id).response(),
                        )?;
                    } else {
                        buffer.queue.push_back(frame);
                    }
                } else {
                    // A finite relay cannot admit this frame for daemon selection.
                    // Decode before refusal so known tools retain their closed DTO shape.
                    let decoded = match &frame {
                        Frame::Line(line) => serde_json::from_slice::<serde_json::Value>(line).ok(),
                        _ => None,
                    };
                    let response = match wire::decode(frame) {
                        Err(error) => Some(error.response()),
                        Ok(Some(request)) if request.id.is_some() => {
                            let id = request.id.clone().expect("checked ID");
                            let duplicate = request
                                .key
                                .as_ref()
                                .is_some_and(|key| pending_duplicate(buffer, key));
                            if duplicate {
                                Some(wire::ProtocolError::new(-32600, id).response())
                            } else {
                                let (event, _) = protocol_event(request, buffer.mode);
                                match event {
                                    crate::mcp::session::Event::Error(error) => {
                                        Some(error.response())
                                    }
                                    crate::mcp::session::Event::Admitted(admission)
                                        if matches!(
                                            admission.action,
                                            crate::mcp::session::Action::Unknown
                                        ) =>
                                    {
                                        Some(wire::ProtocolError::new(-32601, id).response())
                                    }
                                    crate::mcp::session::Event::Admitted(admission)
                                        if matches!(
                                            admission.action,
                                            crate::mcp::session::Action::Call { .. }
                                        ) =>
                                    {
                                        let mut value = decoded.expect("decoded MCP request");
                                        value["id"] = id;
                                        Some(mcp_failure(
                                            &value,
                                            "selection_unavailable",
                                            buffer.workspace,
                                        ))
                                    }
                                    crate::mcp::session::Event::Admitted(_) => {
                                        Some(serde_json::json!({
                                        "jsonrpc":"2.0","id":id,"error":{
                                            "code":-32000,"message":"Relay queue is full",
                                            "data":{"code":"too_many_requests"}}}))
                                    }
                                    crate::mcp::session::Event::Ignore => None,
                                }
                            }
                        }
                        _ => None,
                    };
                    if let Some(response) = response {
                        wire::write_response(stdout, &response)?;
                    }
                }
            }
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Read exactly one complete bounded reply, including when several arrive together.
fn read_mcp_line(
    stream: &mut UnixStream,
    receiver: &FrameReceiver,
    buffer: &mut McpInputBuffer<'_>,
    stdout: &mut impl std::io::Write,
    eof: &mut bool,
    expected_id: Option<&serde_json::Value>,
    receive_bytes: &mut Vec<u8>,
) -> io::Result<Option<serde_json::Value>> {
    use crate::mcp::wire;
    use std::io::Read;
    let mut line = Vec::new();
    let mut canceled = false;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        drain_mcp_input(
            Some(stream),
            receiver,
            buffer,
            stdout,
            eof,
            expected_id,
            &mut canceled,
        )?;
        if *eof || canceled {
            return Ok(None);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon_unavailable: MCP response deadline",
            ));
        }
        let mut buf = [0u8; 4096];
        let chunk = if receive_bytes.is_empty() {
            match stream.read(&mut buf) {
                Ok(0) => {
                    drain_mcp_input(
                        Some(stream),
                        receiver,
                        buffer,
                        stdout,
                        eof,
                        expected_id,
                        &mut canceled,
                    )?;
                    if *eof || canceled {
                        return Ok(None);
                    }
                    return Err(io::Error::new(
                        if line.is_empty() {
                            io::ErrorKind::UnexpectedEof
                        } else {
                            io::ErrorKind::InvalidData
                        },
                        "daemon_unavailable: MCP response interrupted",
                    ));
                }
                Ok(n) => buf[..n].to_vec(),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            }
        } else {
            std::mem::take(receive_bytes)
        };
        let end = chunk.iter().position(|byte| *byte == b'\n');
        let take = end.map_or(chunk.len(), |offset| offset + 1);
        if line.len() + take > wire::RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "MCP response too large",
            ));
        }
        line.extend_from_slice(&chunk[..take]);
        if end.is_some() {
            receive_bytes.extend_from_slice(&chunk[take..]);
            let reply: serde_json::Value = serde_json::from_slice(&line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            drain_mcp_input(
                Some(stream),
                receiver,
                buffer,
                stdout,
                eof,
                expected_id,
                &mut canceled,
            )?;
            if *eof || canceled {
                return Ok(None);
            }
            return Ok(Some(reply));
        }
    }
}

#[cfg(test)]
mod stdin_fence_tests {
    use super::*;
    use crate::mcp::wire::Frame;

    fn pipe() -> [libc::c_int; 2] {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        fds
    }
    fn write_line(fd: libc::c_int, line: &str) {
        assert_eq!(
            unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) },
            line.len() as isize
        );
    }

    #[test]
    fn every_frame_written_before_the_decision_point_is_delivered() {
        let fds = pipe();
        let (frames, fence) = spawn_frame_reader(fds[0]).unwrap();
        for n in 0..50 {
            let line = format!("{{\"n\":{n}}}\n");
            write_line(fds[1], &line);
            let written = fence.written().unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !fence.settled(written) {
                assert!(Instant::now() < deadline, "input never settled");
                thread::sleep(Duration::from_millis(1));
            }
            // Settled through the decision point: the frame is already sent.
            match frames.try_recv() {
                Ok((Frame::Line(bytes), _)) => assert_eq!(bytes, line.trim_end().as_bytes()),
                other => panic!("frame {n} not delivered when settled: {other:?}"),
            }
        }
        unsafe { libc::close(fds[1]) };
        assert!(matches!(
            frames.recv_timeout(Duration::from_secs(5)),
            Ok((Frame::Eof, _))
        ));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fence.settled(u64::MAX) {
            assert!(Instant::now() < deadline, "a closed reader is settled");
            thread::sleep(Duration::from_millis(1));
        }
        unsafe { libc::close(fds[0]) };
    }

    #[test]
    fn later_input_cannot_extend_the_wait() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let fds = pipe();
        let (frames, fence) = spawn_frame_reader(fds[0]).unwrap();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let writer = {
            let stop = stop.clone();
            let fd = fds[1];
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    write_line(fd, "{\"flood\":true}\n");
                }
            })
        };
        thread::sleep(Duration::from_millis(20));
        let written = fence.written().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !fence.settled(written) {
            while frames.try_recv().is_ok() {}
            assert!(
                Instant::now() < deadline,
                "a continuous caller stalled the fence"
            );
            thread::sleep(Duration::from_millis(1));
        }
        stop.store(true, Ordering::Release);
        // Unblock a writer stuck on a full pipe, then let it see `stop`.
        while !writer.is_finished() {
            while frames.try_recv().is_ok() {}
            thread::sleep(Duration::from_millis(1));
        }
        unsafe { libc::close(fds[1]) };
        while !matches!(frames.recv(), Ok((Frame::Eof, _)) | Err(_)) {}
        unsafe { libc::close(fds[0]) };
    }
}

#[cfg(test)]
mod reply_gate_tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Fixture {
        _temp: tempfile::TempDir,
        workspace: crate::store::topology::WorkspaceIdentity,
        fds: [libc::c_int; 2],
        frames: FrameReceiver,
        fence: std::sync::Arc<StdinFence>,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("checkout");
            std::fs::create_dir_all(root.join(".git")).unwrap();
            let workspace =
                crate::store::topology::WorkspaceIdentity::discover_unattached(Some(&root), &root)
                    .unwrap();
            let mut fds = [0; 2];
            assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
            let (frames, fence) = spawn_frame_reader(fds[0]).unwrap();
            Self {
                _temp: temp,
                workspace,
                fds,
                frames,
                fence,
            }
        }
        fn write(&self, line: &str) {
            assert_eq!(
                unsafe { libc::write(self.fds[1], line.as_ptr().cast(), line.len()) },
                line.len() as isize
            );
        }
        /// Run the gate for an active request "A".
        fn gate(&self) -> ReplyGate {
            self.gate_on(&self.frames).0
        }
        /// The gate's decision and the frame it deferred, if any.
        fn gate_on(
            &self,
            frames: &FrameReceiver,
        ) -> (ReplyGate, Option<(crate::mcp::wire::Frame, u64)>) {
            let mut buffer = McpInputBuffer {
                workspace: &self.workspace,
                queue: std::collections::VecDeque::new(),
                cancelled_queued: Vec::new(),
                mode: crate::mcp::session::Mode::Unselected,
                active_id: Some(json!("A")),
                active_key: None,
                cancelled_active: false,
                stdin: self.fence.clone(),
                deferred: None,
                drain_through: None,
            };
            let (mut stream, _daemon) = UnixStream::pair().unwrap();
            let mut stdout = Vec::new();
            let mut eof = false;
            let gate = reply_gate(
                &mut stream,
                frames,
                &mut buffer,
                &mut stdout,
                &mut eof,
                Some(&json!("A")),
            )
            .unwrap();
            (gate, buffer.deferred.take())
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Close the write end first and let the reader reach EOF. Closing a
            // descriptor its reader still polls would let a later fixture's
            // pipe reuse the number and be read by this stale reader.
            unsafe { libc::close(self.fds[1]) };
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self.fence.closed.load(Ordering::SeqCst) && Instant::now() < deadline {
                while self.frames.try_recv().is_ok() {}
                thread::sleep(Duration::from_millis(1));
            }
            unsafe { libc::close(self.fds[0]) };
        }
    }
    const CANCEL_A: &str = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":\"A\"}}\n";

    #[test]
    fn a_cancel_written_before_the_decision_wins_even_if_unread() {
        for _ in 0..50 {
            let fixture = Fixture::new();
            fixture.write(CANCEL_A);
            assert!(matches!(fixture.gate(), ReplyGate::Cancelled));
        }
    }

    #[test]
    fn a_cancel_written_just_after_the_decision_cannot_suppress_the_reply() {
        for _ in 0..50 {
            let fixture = Fixture::new();
            // A complete non-cancel frame is pending at the decision point.
            fixture.write("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n");
            let fd = fixture.fds[1];
            *fixture.fence.after_sample.lock().unwrap() = Some(Box::new(move || {
                assert_eq!(
                    unsafe { libc::write(fd, CANCEL_A.as_ptr().cast(), CANCEL_A.len()) },
                    CANCEL_A.len() as isize
                );
            }));
            assert!(matches!(fixture.gate(), ReplyGate::Write));
        }
    }

    #[test]
    fn unmeasurable_pending_input_is_an_unknown_outcome() {
        let fixture = Fixture::new();
        fixture.write(CANCEL_A);
        fixture.fence.fail_fionread.store(true, Ordering::SeqCst);
        assert!(matches!(fixture.gate(), ReplyGate::Unknown));
    }

    #[test]
    fn a_later_frame_queued_at_the_gate_is_deferred_unhandled() {
        const LATER: &str = "{\"jsonrpc\":\"2.0\",\"id\":\"B\",\"method\":\"tools/list\"}\n";
        for _ in 0..50 {
            let fixture = Fixture::new();
            // A real pre-decision frame is pending in the pipe.
            fixture.write("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n");
            // Right after the decision offset is sampled, a later frame is
            // written and fully delivered to the channel before the gate drains.
            let fd = fixture.fds[1];
            let fence = fixture.fence.clone();
            let decided = std::sync::Arc::new(std::sync::Mutex::new(None));
            let sampled = decided.clone();
            *fixture.fence.after_sample.lock().unwrap() = Some(Box::new(move || {
                use std::sync::atomic::Ordering::SeqCst;
                *sampled.lock().unwrap() = Some(fence.read_total.load(SeqCst));
                assert_eq!(
                    unsafe { libc::write(fd, LATER.as_ptr().cast(), LATER.len()) },
                    LATER.len() as isize
                );
                let through = fence.written().unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                while !fence.settled(through) {
                    assert!(Instant::now() < deadline, "later frame never delivered");
                    thread::sleep(Duration::from_millis(1));
                }
            }));
            let (gate, deferred) = fixture.gate_on(&fixture.frames);
            assert!(matches!(gate, ReplyGate::Write));
            match deferred {
                Some((crate::mcp::wire::Frame::Line(bytes), end)) => {
                    assert_eq!(bytes, LATER.trim_end().as_bytes());
                    let total = decided.lock().unwrap().unwrap();
                    assert!(end > total, "the deferred frame ends after the decision");
                }
                other => panic!("the later frame was not deferred: {other:?}"),
            }
            // Nothing written after the decision was handled by the gate.
            assert!(fixture.frames.try_recv().is_err());
        }
    }

    #[test]
    fn later_input_cannot_stall_the_gate() {
        // Nothing was written before the decision, but a producer keeps the
        // frame channel permanently non-empty with later input. An unbounded
        // drain would never return; the gate must decide at once.
        let (sender, frames) = std::sync::mpsc::channel();
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let (queued, first) = std::sync::mpsc::channel();
        let producer = {
            let stop = stop.clone();
            thread::spawn(move || {
                let mut queued = Some(queued);
                let line = b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}".to_vec();
                while !stop.load(Ordering::Acquire) {
                    // Every frame ends after the empty decision offset.
                    if sender
                        .send((crate::mcp::wire::Frame::Line(line.clone()), u64::MAX))
                        .is_err()
                    {
                        break;
                    }
                    if let Some(queued) = queued.take() {
                        let _ = queued.send(());
                    }
                }
            })
        };
        // At least one later frame is queued before the gate runs.
        first.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done, decided) = std::sync::mpsc::channel();
        let gate = thread::spawn(move || {
            let fixture = Fixture::new();
            let (gate, deferred) = fixture.gate_on(&frames);
            let _ = done.send(matches!(gate, ReplyGate::Write) && deferred.is_some());
        });
        let outcome = decided.recv_timeout(Duration::from_secs(5));
        stop.store(true, Ordering::Release);
        producer.join().unwrap();
        assert_eq!(outcome, Ok(true), "later input stalled the reply gate");
        gate.join().unwrap();
    }
}

#[cfg(test)]
mod relay_failure_tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn unresolved_tool_input_keeps_closed_application_and_selection_precedence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let workspace =
            crate::store::topology::WorkspaceIdentity::discover_unattached(Some(&root), &root)
                .unwrap();
        for (selection, code, reason) in [
            (json!(42), "invalid_request", None),
            (json!("x".repeat(4097)), "invalid_request", None),
            (
                json!("../relative"),
                "workspace_selection_failed",
                Some("not_absolute"),
            ),
        ] {
            let request = json!({"jsonrpc":"2.0","id":"validation",
                "method":"tools/call","params":{"name":"trellis_workspace_describe",
                "arguments":{"schemaVersion":1,"workspace":selection},
                "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities":{}}}});
            let reply = mcp_failure(&request, "daemon_unavailable", &workspace);
            assert_eq!(reply["id"], "validation", "{reply}");
            assert_eq!(reply["result"]["isError"], true, "{reply}");
            let envelope = &reply["result"]["structuredContent"];
            assert_eq!(envelope["error"]["code"], code, "{reply}");
            assert_eq!(envelope["error"]["reason"].as_str(), reason, "{reply}");
            assert_eq!(envelope["requestId"], "validation");
            assert!(envelope["error"]["attemptedWorkspace"].is_object());
            assert!(envelope.get("workspace").is_none());
            assert!(envelope.get("catchingUp").is_none());
            assert_eq!(
                serde_json::from_str::<Value>(
                    reply["result"]["content"][0]["text"].as_str().unwrap()
                )
                .unwrap(),
                *envelope
            );
        }
        let legacy = json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"trellis_workspace_describe","arguments":{"schemaVersion":1}}});
        let reply = mcp_failure(&legacy, "daemon_unavailable", &workspace);
        assert_eq!(reply["result"]["structuredContent"]["requestId"], 2);
        assert_eq!(
            reply["result"]["structuredContent"]["error"]["reason"],
            "unavailable"
        );
        assert!(reply["result"].get("resultType").is_none(), "{reply}");
    }
}
