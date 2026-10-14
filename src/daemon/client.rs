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
            // Frames carry the relay's own ID (Decision 0009 §7).
            "relay": true,
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
    /// Frames taken from the channel for later, each with its acceptance.
    queue: std::collections::VecDeque<(crate::mcp::wire::Frame, Instant)>,
    cancelled_queued: Vec<serde_json::Value>,
    mode: crate::mcp::session::Mode,
    active_id: Option<serde_json::Value>,
    active_key: Option<crate::mcp::wire::IdKey>,
    cancelled_active: bool,
    stdin: std::sync::Arc<StdinFence>,
    /// A frame taken from the channel but not yet handled; it precedes every
    /// frame still in the channel.
    deferred: Option<(crate::mcp::wire::Frame, u64, Instant)>,
    /// While set, `drain_mcp_input` handles only frames whose bytes end at or
    /// before this stdin offset and defers the first later one.
    drain_through: Option<u64>,
    /// Cancellations drained for requests already forwarded to the daemon; the
    /// relay loop settles them (Decision 0009 §7).
    cancels: Vec<serde_json::Value>,
    /// IDs of requests forwarded to the daemon and not yet decided.
    outstanding: Vec<crate::mcp::wire::IdKey>,
}

/// Frames from the stdin reader, each with the stdin offset where it ends and
/// the instant the reader completed it (protocol acceptance, Decision 0009 §7).
type FrameReceiver = std::sync::mpsc::Receiver<(crate::mcp::wire::Frame, u64, Instant)>;

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
    #[cfg(test)]
    fail_poll: std::sync::atomic::AtomicBool,
    /// While set, the reader waits after `poll` wakes it, leaving input unread.
    #[cfg(test)]
    hold_reads: std::sync::atomic::AtomicBool,
    /// Runs once between measuring pending bytes and checking for hang-up.
    #[cfg(test)]
    between_measures: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
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
            fail_poll: Default::default(),
            #[cfg(test)]
            hold_reads: Default::default(),
            #[cfg(test)]
            between_measures: Default::default(),
            #[cfg(test)]
            after_sample: Default::default(),
        }
    }

    /// The decision offset: how far the client's input had reached, or `None`
    /// when that cannot be measured, so the decision cannot be fenced.
    ///
    /// Under the reader's lock, measure pending bytes, then check for hang-up.
    /// EOF occupies its own position, one past the last byte. Without hang-up,
    /// the decision instant is the measurement. With hang-up (macOS and Linux
    /// both report POLLHUP once no writer remains, even with bytes buffered),
    /// the client closed before the check; a closed pipe cannot grow, so
    /// measure again and fence EOF past every byte written before the close.
    fn written(&self) -> Option<u64> {
        use std::sync::atomic::Ordering::SeqCst;
        let sampled = {
            let _io = self.io.lock().unwrap();
            (|| {
                let pending = self.pending()?;
                #[cfg(test)]
                {
                    let hook = self.between_measures.lock().unwrap().take();
                    if let Some(hook) = hook {
                        hook();
                    }
                }
                let read = self.read_total.load(SeqCst);
                if self.hung_up()? {
                    Some(read + self.pending()? + 1)
                } else {
                    Some(read + pending)
                }
            })()
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

    /// Unread bytes in the pipe.
    fn pending(&self) -> Option<u64> {
        #[cfg(test)]
        if self.fail_fionread.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let mut pending: libc::c_int = 0;
        (unsafe { libc::ioctl(self.fd, libc::FIONREAD, &mut pending) } == 0)
            .then_some(pending.max(0) as u64)
    }

    /// Whether no writer remains. A failed check is unmeasurable, never "open".
    fn hung_up(&self) -> Option<bool> {
        #[cfg(test)]
        if self.fail_poll.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let mut ready = libc::pollfd {
            fd: self.fd,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            let rc = unsafe { libc::poll(&mut ready, 1, 0) };
            if rc >= 0 {
                return Some(rc > 0 && ready.revents & libc::POLLHUP != 0);
            }
            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                return None;
            }
        }
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
        #[cfg(test)]
        while self.fence.hold_reads.load(SeqCst) {
            thread::sleep(Duration::from_millis(1));
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

/// Wakes the relay's single thread for stdin, daemon replies or a deadline.
/// A sticky flag: a ring is never lost, only coalesced.
#[derive(Default)]
struct Doorbell {
    rung: std::sync::Mutex<bool>,
    changed: std::sync::Condvar,
}

impl Doorbell {
    fn ring(&self) {
        *self.rung.lock().unwrap() = true;
        self.changed.notify_one();
    }

    /// Block until rung or `deadline` passes (`None`: until rung).
    fn wait(&self, deadline: Option<Instant>) {
        let mut rung = self.rung.lock().unwrap();
        while !*rung {
            match deadline {
                None => rung = self.changed.wait(rung).unwrap(),
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    rung = self.changed.wait_timeout(rung, deadline - now).unwrap().0;
                }
            }
        }
        *rung = false;
    }
}

/// Spawn the bounded frame reader for one descriptor. Each frame carries the
/// stdin offset just past its last byte.
#[cfg(test)]
fn spawn_frame_reader(
    fd: std::os::fd::RawFd,
) -> io::Result<(FrameReceiver, std::sync::Arc<StdinFence>)> {
    spawn_ringing_frame_reader(fd, None)
}

/// As `spawn_frame_reader`, ringing `doorbell` after every frame it sends.
fn spawn_ringing_frame_reader(
    fd: std::os::fd::RawFd,
    doorbell: Option<std::sync::Arc<Doorbell>>,
) -> io::Result<(FrameReceiver, std::sync::Arc<StdinFence>)> {
    use crate::mcp::wire::{self, Frame};
    use std::sync::atomic::Ordering::SeqCst;
    let fence = std::sync::Arc::new(StdinFence::new(fd));
    let (sender, receiver) = std::sync::mpsc::sync_channel::<(Frame, u64, Instant)>(16);
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
                let accepted = Instant::now();
                let consumed = reader_fence.read_total.load(SeqCst) - reader.buffer().len() as u64;
                // EOF sits one past the last byte, so it is inside a decision
                // fence only when the client had closed before that decision.
                let end = consumed + u64::from(matches!(frame, Frame::Eof));
                let last = matches!(frame, Frame::Eof);
                let sent = sender.send((frame, end, accepted));
                if let Some(doorbell) = &doorbell {
                    doorbell.ring();
                }
                if sent.is_err() || last {
                    break;
                }
            }
            reader_fence.closed.store(true, SeqCst);
        })?;
    Ok((receiver, fence))
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
///
/// Settling ends by `bound`: a call's own deadline (acceptance + 5 s), or the
/// reply bound for other requests. Input not settled by then makes the outcome
/// `outcome_unknown`.
fn reply_gate(
    receiver: &FrameReceiver,
    buffer: &mut McpInputBuffer<'_>,
    stdout: &mut impl std::io::Write,
    eof: &mut bool,
    active_id: Option<&serde_json::Value>,
    bound: Instant,
) -> io::Result<ReplyGate> {
    let Some(written) = buffer.stdin.written() else {
        return Ok(ReplyGate::Unknown);
    };
    let deadline = bound;
    let mut canceled = false;
    loop {
        // Sampled first: frames within `written` sent before settling are then
        // all in the channel for the drain below.
        let settled = buffer.stdin.settled(written);
        buffer.drain_through = Some(written);
        let drained = drain_mcp_input(
            None,
            receiver,
            buffer,
            stdout,
            eof,
            active_id,
            &mut canceled,
        );
        buffer.drain_through = None;
        drained?;
        // A cancel for this ID that an earlier pass drained also counts.
        let pending_cancel =
            active_id.is_some_and(|id| buffer.cancels.iter().any(|cancel| cancel == id));
        if *eof || canceled || pending_cancel || buffer.cancelled_active {
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

/// Decide one outcome for `value`'s ID at the #127 decision point: written if
/// no cancel or end of input settled before it, `outcome_unknown` if input
/// could not be settled by `bound`.
fn decide(
    receiver: &FrameReceiver,
    buffer: &mut McpInputBuffer<'_>,
    stdout: &mut impl std::io::Write,
    eof: &mut bool,
    value: &serde_json::Value,
    bound: Instant,
    response: &serde_json::Value,
) -> io::Result<()> {
    match reply_gate(receiver, buffer, stdout, eof, value.get("id"), bound)? {
        ReplyGate::Write => crate::mcp::wire::write_response(stdout, response),
        ReplyGate::Cancelled => Ok(()),
        ReplyGate::Unknown => crate::mcp::wire::write_response(
            stdout,
            &mcp_failure(value, "outcome_unknown", buffer.workspace),
        ),
    }
}

/// The settling bound for deciding `request`'s outcome now.
fn decision_bound(request: &Forwarded) -> Instant {
    if request.call {
        request.deadline
    } else {
        Instant::now() + RELAY_REPLY_BOUND
    }
}

/// A request forwarded to the daemon under the relay's own ID and not yet
/// decided (Decision 0009 §7).
struct Forwarded {
    /// The client's request, with the client's ID.
    value: serde_json::Value,
    /// Its exact bytes, as the client wrote them.
    line: Vec<u8>,
    key: Option<crate::mcp::wire::IdKey>,
    call: bool,
    /// Calls: the strict client-facing reply bound, from the relay's
    /// acceptance. Other requests: the bound for the daemon's reply.
    deadline: Instant,
    /// An interrupted read is replayed once, never more.
    retried: bool,
}

/// One daemon connection generation: replies from it carry its number.
struct DaemonConnection {
    stream: UnixStream,
    generation: u64,
}

/// A daemon reply: its relay ID and the unchanged MCP line, or a broken
/// connection.
type DaemonReply = (u64, io::Result<(u64, Vec<u8>)>);

/// The bound for a daemon reply to a request that is not a tool call.
const RELAY_REPLY_BOUND: Duration = Duration::from_secs(10);
/// Outstanding requests that are not tool calls, per connection.
const RELAY_OUTSTANDING_REQUESTS: usize = 16;
/// Outstanding tool calls per connection: a fifth is refused without queueing.
const RELAY_OUTSTANDING_CALLS: usize = 4;
/// The relay ID of the synthetic legacy handshake replayed on reconnect.
const SETUP_RELAY_ID: u64 = 0;

/// Read daemon replies on their own thread, so a reply never waits for stdin.
fn spawn_reply_reader(
    stream: &UnixStream,
    generation: u64,
    replies: std::sync::mpsc::Sender<DaemonReply>,
    doorbell: std::sync::Arc<Doorbell>,
) -> io::Result<()> {
    use io::BufRead;
    let reader = stream.try_clone()?;
    thread::Builder::new()
        .name("mcp-daemon-replies".into())
        .spawn(move || {
            let mut reader = io::BufReader::new(reader);
            loop {
                let mut line = Vec::new();
                let read = (|| -> io::Result<Option<Vec<u8>>> {
                    loop {
                        let available = match reader.fill_buf() {
                            Ok(available) => available,
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
                        };
                        if available.is_empty() {
                            return if line.is_empty() {
                                Ok(None)
                            } else {
                                Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "daemon_unavailable: MCP response interrupted",
                                ))
                            };
                        }
                        let end = available.iter().position(|&b| b == b'\n');
                        let take = end.map_or(available.len(), |at| at + 1);
                        if line.len() + take > crate::mcp::wire::RESPONSE_BYTES + 32 {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "MCP response too large",
                            ));
                        }
                        line.extend_from_slice(&available[..take]);
                        reader.consume(take);
                        if end.is_some() {
                            line.pop();
                            return Ok(Some(line));
                        }
                    }
                })();
                let reply = match read {
                    Ok(Some(line)) => crate::mcp::wire::split_relay_prefix(&line)
                        .map(|(relay, rest)| (relay, rest.to_vec())),
                    Ok(None) => Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "daemon_unavailable: MCP connection closed",
                    )),
                    Err(error) => Err(error),
                };
                let last = reply.is_err();
                let sent = replies.send((generation, reply));
                doorbell.ring();
                if last || sent.is_err() {
                    break;
                }
            }
        })?;
    Ok(())
}

/// The strict client-facing call deadline: `CALL_DEADLINE`, or a shorter one
/// a test sets so it need not wait real seconds.
fn relay_call_deadline() -> Duration {
    #[cfg(feature = "test-support")]
    if let Some(millis) = std::env::var("TRELLIS_TEST_MCP_RELAY_DEADLINE_MS")
        .ok()
        .and_then(|value| value.parse().ok())
    {
        return Duration::from_millis(millis);
    }
    crate::mcp::CALL_DEADLINE
}

/// A tool call's admission refusal made by the relay itself, in the closed
/// pre-resolution shape; application validation still takes precedence.
fn relay_admission_failure(
    value: &serde_json::Value,
    code: &str,
    workspace: &crate::store::topology::WorkspaceIdentity,
) -> serde_json::Value {
    use serde_json::json;
    let id = value.get("id").cloned().unwrap_or_default();
    let name = value
        .pointer("/params/name")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let arguments = value.pointer("/params/arguments");
    if crate::mcp::tools::validate(name, arguments).is_err() {
        return mcp_failure(value, "daemon_unavailable", workspace);
    }
    let attempted = arguments
        .and_then(serde_json::Value::as_object)
        .and_then(|object| object.get("workspace"))
        .cloned()
        .unwrap_or_else(|| json!(workspace.root));
    let modern = value.pointer("/params/_meta").is_some();
    json!({"jsonrpc":"2.0","id":id,"result":crate::mcp::tools::result(
        crate::mcp::tools::admission_failure(&id, code, &attempted), modern)})
}

/// The relay between the agent's stdio and the daemon (Decision 0009 §7). It
/// forwards each request at once under its own relay ID, decides each
/// outcome per client ID at the #127 decision point, owns the strict
/// client-facing call deadline, and replays an interrupted read once.
pub fn relay_stdio(
    stream: UnixStream,
    workspace: &crate::store::topology::WorkspaceIdentity,
    mut reconnect: impl FnMut() -> io::Result<UnixStream>,
) -> io::Result<()> {
    use crate::mcp::wire::{self, Frame};
    use std::collections::BTreeMap;
    use std::io::Write;
    let doorbell = std::sync::Arc::new(Doorbell::default());
    let (receiver, stdin_fence) = spawn_ringing_frame_reader(0, Some(doorbell.clone()))?;
    let (reply_sender, replies) = std::sync::mpsc::channel::<DaemonReply>();
    let mut generation = 0u64;
    let open = |stream: io::Result<UnixStream>, generation: u64| -> io::Result<DaemonConnection> {
        let stream = attach_mcp(stream?, workspace)?;
        stream.set_read_timeout(None)?;
        spawn_reply_reader(&stream, generation, reply_sender.clone(), doorbell.clone())?;
        Ok(DaemonConnection { stream, generation })
    };
    let mut daemon = Some(open(Ok(stream), generation)?);
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
        cancels: Vec::new(),
        outstanding: Vec::new(),
    };
    let mut stdout = io::stdout().lock();
    let mut forwarded: BTreeMap<u64, Forwarded> = BTreeMap::new();
    let mut next_relay = SETUP_RELAY_ID + 1;
    let mut legacy_init: Option<Vec<u8>> = None;
    let mut legacy_ready = false;
    let mut eof = false;
    let call_deadline = relay_call_deadline();

    // Write one frame under a relay ID, (re)connecting first if needed. A
    // new connection after a completed legacy handshake replays it.
    let mut send = |daemon: &mut Option<DaemonConnection>,
                    generation: &mut u64,
                    legacy: (Option<&Vec<u8>>, bool),
                    relay: u64,
                    line: &[u8]|
     -> bool {
        if daemon.is_none() {
            *generation += 1;
            let Ok(mut connection) = open(reconnect(), *generation) else {
                return false;
            };
            if let (Some(init), true) = legacy {
                let mut setup = format!("{SETUP_RELAY_ID} ").into_bytes();
                setup.extend_from_slice(init);
                setup.extend_from_slice(b"\n");
                setup.extend_from_slice(
                    format!("{SETUP_RELAY_ID} {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n")
                        .as_bytes(),
                );
                if connection.stream.write_all(&setup).is_err() {
                    return false;
                }
            }
            *daemon = Some(connection);
        }
        let connection = daemon.as_mut().expect("connected");
        if line.is_empty() {
            // Connect only.
            return true;
        }
        let mut framed = format!("{relay} ").into_bytes();
        framed.extend_from_slice(line);
        framed.push(b'\n');
        if connection.stream.write_all(&framed).is_err() {
            if let Some(lost) = daemon.take() {
                let _ = lost.stream.shutdown(std::net::Shutdown::Both);
            }
            return false;
        }
        true
    };

    loop {
        // Daemon replies and broken connections.
        while let Ok((reply_generation, reply)) = replies.try_recv() {
            if daemon.as_ref().map(|d| d.generation) != Some(reply_generation) {
                continue;
            }
            match reply {
                Ok((SETUP_RELAY_ID, _)) => {}
                Ok((relay, line)) => {
                    // Cancels drained since (by another decision) drop their
                    // requests first, so a cancelled reply is never written.
                    settle_cancels(&mut buffer, &mut forwarded, &mut daemon);
                    let Some(request) = forwarded.remove(&relay) else {
                        // Decided at its deadline or cancelled: never written.
                        continue;
                    };
                    if let Some(key) = &request.key {
                        buffer.outstanding.retain(|held| held != key);
                    }
                    let id_matches = serde_json::from_slice::<serde_json::Value>(&line)
                        .ok()
                        .is_some_and(|reply| reply.get("id") == request.value.get("id"));
                    if !id_matches {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "daemon MCP reply ID mismatch",
                        ));
                    }
                    crate::mcp::phase_hook("relay_before_write")?;
                    // A call's reply that arrives after its deadline is not
                    // written: the relay's decision is deadline_exceeded.
                    let late = request.call && Instant::now() >= request.deadline;
                    let gate = reply_gate(
                        &receiver,
                        &mut buffer,
                        &mut stdout,
                        &mut eof,
                        request.value.get("id"),
                        decision_bound(&request),
                    )?;
                    match gate {
                        ReplyGate::Write if late => wire::write_response(
                            &mut stdout,
                            &relay_admission_failure(
                                &request.value,
                                "deadline_exceeded",
                                workspace,
                            ),
                        )?,
                        ReplyGate::Write => {
                            crate::mcp::phase_hook("relay_after_decision")?;
                            // The daemon's exact line, under the client's ID.
                            let mut out = line;
                            out.push(b'\n');
                            stdout.write_all(&out)?;
                            stdout.flush()?;
                        }
                        ReplyGate::Cancelled => {}
                        ReplyGate::Unknown => wire::write_response(
                            &mut stdout,
                            &mcp_failure(&request.value, "outcome_unknown", workspace),
                        )?,
                    }
                }
                Err(error) => {
                    // The connection broke: settle every forwarded request.
                    let partial = error.kind() == io::ErrorKind::InvalidData;
                    if let Some(lost) = daemon.take() {
                        let _ = lost.stream.shutdown(std::net::Shutdown::Both);
                    }
                    let lost: Vec<(u64, Forwarded)> =
                        std::mem::take(&mut forwarded).into_iter().collect();
                    buffer.outstanding.clear();
                    for (relay, mut request) in lost {
                        // Never replay a truncated reply's request or one
                        // already replayed; never write for a cancelled one.
                        let replay = !partial && !request.retried && !eof;
                        if replay {
                            request.retried = true;
                            if send(
                                &mut daemon,
                                &mut generation,
                                (legacy_init.as_ref(), legacy_ready),
                                relay,
                                &request.line,
                            ) {
                                if let Some(key) = &request.key {
                                    buffer.outstanding.push(key.clone());
                                }
                                forwarded.insert(relay, request);
                                continue;
                            }
                        }
                        if request.value.get("id").is_some() {
                            let bound = decision_bound(&request);
                            decide(
                                &receiver,
                                &mut buffer,
                                &mut stdout,
                                &mut eof,
                                &request.value,
                                bound,
                                &mcp_failure(&request.value, "daemon_unavailable", workspace),
                            )?;
                        }
                    }
                }
            }
            settle_cancels(&mut buffer, &mut forwarded, &mut daemon);
        }

        // Calls past the strict client-facing deadline, and daemon replies
        // overdue for other requests.
        let now = Instant::now();
        let overdue: Vec<u64> = forwarded
            .iter()
            .filter(|(_, request)| request.deadline <= now)
            .map(|(relay, _)| *relay)
            .collect();
        for relay in overdue {
            let request = forwarded.remove(&relay).expect("overdue request");
            if let Some(key) = &request.key {
                buffer.outstanding.retain(|held| held != key);
            }
            if !request.call {
                // A non-call reply is overdue: treat the connection as broken.
                if let Some(lost) = daemon.take() {
                    let _ = lost.stream.shutdown(std::net::Shutdown::Both);
                }
                decide(
                    &receiver,
                    &mut buffer,
                    &mut stdout,
                    &mut eof,
                    &request.value,
                    Instant::now() + RELAY_REPLY_BOUND,
                    &mcp_failure(&request.value, "daemon_unavailable", workspace),
                )?;
                continue;
            }
            // The call's own deadline: settling ends by it.
            decide(
                &receiver,
                &mut buffer,
                &mut stdout,
                &mut eof,
                &request.value,
                request.deadline,
                &relay_admission_failure(&request.value, "deadline_exceeded", workspace),
            )?;
            // Stop the daemon's worker; a late reply is dropped here.
            if let (Some(connection), Some(id)) = (daemon.as_mut(), request.value.get("id")) {
                let cancel = serde_json::json!({"jsonrpc":"2.0",
                    "method":"notifications/cancelled","params":{"requestId":id}});
                let _ = connection
                    .stream
                    .write_all(format!("{SETUP_RELAY_ID} {cancel}\n").as_bytes());
            }
            settle_cancels(&mut buffer, &mut forwarded, &mut daemon);
        }

        // One stdin frame at a time.
        let next = if let Some(queued) = buffer.queue.pop_front() {
            Some(queued)
        } else if let Some((frame, _, accepted)) = buffer.deferred.take() {
            Some((frame, accepted))
        } else {
            receiver
                .try_recv()
                .ok()
                .map(|(frame, _, accepted)| (frame, accepted))
        };
        let Some((frame, accepted)) = next else {
            if eof {
                break;
            }
            doorbell.wait(forwarded.values().map(|request| request.deadline).min());
            continue;
        };
        let line = match frame {
            Frame::Eof => {
                eof = true;
                continue;
            }
            Frame::Oversized => {
                wire::write_response(
                    &mut stdout,
                    &wire::ProtocolError::new(-32600, serde_json::Value::Null).response(),
                )?;
                continue;
            }
            Frame::Line(line) => line,
        };
        if cancellation_id(&Frame::Line(line.clone())).is_some() {
            // A cancel for a forwarded request goes to the daemon; one for a
            // locally queued request drops it.
            let id = cancellation_id(&Frame::Line(line.clone())).expect("checked cancel");
            buffer.cancels.push(id);
            settle_cancels(&mut buffer, &mut forwarded, &mut daemon);
            continue;
        }
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
        let key = admitted.key.clone();
        let method = value
            .get("method")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
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
        if let Some(key) = &key
            && buffer.outstanding.contains(key)
        {
            // An outstanding ID cannot be reused.
            wire::write_response(
                &mut stdout,
                &wire::ProtocolError::new(-32600, value["id"].clone()).response(),
            )?;
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
        if method == "initialize" && has_id {
            legacy_init = Some(line.clone());
            legacy_ready = false;
        }
        if method == "notifications/initialized" && mode == crate::mcp::session::Mode::LegacyReady {
            legacy_ready = true;
        }
        let call = method == "tools/call";
        // From the relay's protocol acceptance: when stdin completed the frame.
        let deadline = accepted
            + if call {
                call_deadline
            } else {
                RELAY_REPLY_BOUND
            };
        if has_id {
            let calls = forwarded.values().filter(|request| request.call).count();
            let refusal = if call && calls >= RELAY_OUTSTANDING_CALLS {
                Some(relay_admission_failure(
                    &value,
                    "too_many_requests",
                    workspace,
                ))
            } else if !call && forwarded.len() - calls >= RELAY_OUTSTANDING_REQUESTS {
                Some(
                    serde_json::json!({"jsonrpc":"2.0","id":value["id"],"error":{
                    "code":-32000,"message":"Relay queue is full",
                    "data":{"code":"too_many_requests"}}}),
                )
            } else {
                None
            };
            if let Some(refusal) = refusal {
                let bound = if call {
                    deadline
                } else {
                    Instant::now() + RELAY_REPLY_BOUND
                };
                decide(
                    &receiver,
                    &mut buffer,
                    &mut stdout,
                    &mut eof,
                    &value,
                    bound,
                    &refusal,
                )?;
                continue;
            }
        }
        let relay = next_relay;
        next_relay += 1;
        let request = Forwarded {
            value,
            line,
            key,
            call,
            deadline,
            retried: false,
        };
        // A completed handshake is replayed only before later requests.
        let legacy = (legacy_init.as_ref(), legacy_ready && method != "initialize");
        // Connect first (a reconnect may take a while), then settle input
        // that arrived meanwhile: a cancel already written drops this request
        // before it reaches the daemon.
        let connected =
            daemon.is_some() || send(&mut daemon, &mut generation, legacy, SETUP_RELAY_ID, b"");
        let mut cancelled = false;
        drain_mcp_input(
            None,
            &receiver,
            &mut buffer,
            &mut stdout,
            &mut eof,
            request.value.get("id"),
            &mut cancelled,
        )?;
        settle_cancels(&mut buffer, &mut forwarded, &mut daemon);
        if eof || (cancelled && request.value.get("id").is_some()) {
            // End of input cancels outstanding work; a cancelled request is
            // never sent.
            continue;
        }
        if call && has_id && Instant::now() >= request.deadline {
            // Its deadline passed before it could be forwarded.
            decide(
                &receiver,
                &mut buffer,
                &mut stdout,
                &mut eof,
                &request.value,
                request.deadline,
                &relay_admission_failure(&request.value, "deadline_exceeded", workspace),
            )?;
            continue;
        }
        // One reconnect attempt per request: a failed attach is final.
        let sent = connected && send(&mut daemon, &mut generation, legacy, relay, &request.line);
        if request.value.get("id").is_none() {
            // A notification: nothing to decide.
            continue;
        }
        if sent {
            if let Some(key) = &request.key {
                buffer.outstanding.push(key.clone());
            }
            forwarded.insert(relay, request);
            continue;
        }
        // Could not reach the daemon: never replay a request that may have
        // been delivered; a read that certainly was not is retried once.
        let mut request = request;
        if connected && !request.retried {
            // The connection broke on write: the request was not delivered,
            // so it is retried once on a new connection.
            request.retried = true;
            if send(
                &mut daemon,
                &mut generation,
                (legacy_init.as_ref(), legacy_ready && method != "initialize"),
                relay,
                &request.line,
            ) {
                if let Some(key) = &request.key {
                    buffer.outstanding.push(key.clone());
                }
                forwarded.insert(relay, request);
                continue;
            }
        }
        let bound = decision_bound(&request);
        decide(
            &receiver,
            &mut buffer,
            &mut stdout,
            &mut eof,
            &request.value,
            bound,
            &mcp_failure(&request.value, "daemon_unavailable", workspace),
        )?;
    }
    if let Some(connection) = daemon {
        let _ = connection.stream.shutdown(std::net::Shutdown::Write);
    }
    Ok(())
}

/// Settle cancellations drained from stdin: a forwarded request is dropped
/// here, and the daemon is told so it stops the request's worker.
fn settle_cancels(
    buffer: &mut McpInputBuffer<'_>,
    forwarded: &mut std::collections::BTreeMap<u64, Forwarded>,
    daemon: &mut Option<DaemonConnection>,
) {
    use std::io::Write;
    for id in std::mem::take(&mut buffer.cancels) {
        let matched: Vec<u64> = forwarded
            .iter()
            .filter(|(_, request)| request.value.get("id") == Some(&id))
            .map(|(relay, _)| *relay)
            .collect();
        for relay in matched {
            if let Some(request) = forwarded.remove(&relay)
                && let Some(key) = &request.key
            {
                buffer.outstanding.retain(|held| held != key);
            }
        }
        if let Some(connection) = daemon.as_mut() {
            let cancel = serde_json::json!({"jsonrpc":"2.0",
                "method":"notifications/cancelled","params":{"requestId":id}});
            let _ = connection
                .stream
                .write_all(format!("{SETUP_RELAY_ID} {cancel}\n").as_bytes());
        }
    }
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
        || buffer.outstanding.contains(key)
        || buffer.queue.iter().any(|(queued, _)| {
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

    use std::sync::mpsc::TryRecvError;
    loop {
        let next = match buffer.deferred.take() {
            Some(entry) => Ok(entry),
            None => receiver.try_recv(),
        };
        let next = match next {
            Ok((frame, end, accepted))
                if buffer.drain_through.is_some_and(|through| end > through) =>
            {
                buffer.deferred = Some((frame, end, accepted));
                break;
            }
            other => other.map(|(frame, _, accepted)| (frame, accepted)),
        };
        match next {
            Ok((Frame::Eof, _)) => {
                *eof = true;
                return Ok(());
            }
            Ok((frame, accepted)) => {
                if let Some(id) = cancellation_id(&frame) {
                    let _ = &mut stream;
                    if active_id == Some(&id) {
                        // Decided per reply: never a sticky connection flag.
                        *canceled = true;
                    } else if buffer.active_id.as_ref() == Some(&id) {
                        buffer.cancelled_active = true;
                    } else if buffer.queue.iter().any(|(frame, _)| {
                        matches!(frame,
                        Frame::Line(line) if wire::decode(Frame::Line(line.clone())).ok()
                            .flatten().and_then(|request| request.id).as_ref() == Some(&id))
                    }) && !buffer.cancelled_queued.contains(&id)
                    {
                        buffer.cancelled_queued.push(id);
                        continue;
                    }
                    // The relay loop forwards it to the daemon for any
                    // forwarded request, including the one being decided.
                    buffer.cancels.push(id);
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
                        buffer.queue.push_back((frame, accepted));
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
                                        Some(relay_admission_failure(
                                            &value,
                                            "too_many_requests",
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

#[cfg(test)]
mod stdin_fence_tests {
    use super::*;
    use crate::mcp::wire::Frame;

    /// A pipe that child processes spawned by concurrent tests cannot inherit:
    /// an inherited write end would hold off EOF after the test closes it.
    pub(super) fn pipe() -> [libc::c_int; 2] {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        for fd in fds {
            assert_eq!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                0
            );
        }
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
            let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
            while !fence.settled(written) {
                assert!(Instant::now() < deadline, "input never settled");
                thread::sleep(Duration::from_millis(1));
            }
            // Settled through the decision point: the frame is already sent.
            match frames.try_recv() {
                Ok((Frame::Line(bytes), _, _)) => assert_eq!(bytes, line.trim_end().as_bytes()),
                other => panic!("frame {n} not delivered when settled: {other:?}"),
            }
        }
        unsafe { libc::close(fds[1]) };
        assert!(matches!(
            frames.recv_timeout(crate::testing::HANG_BACKSTOP),
            Ok((Frame::Eof, _, _))
        ));
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
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
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
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
        while !matches!(frames.recv(), Ok((Frame::Eof, _, _)) | Err(_)) {}
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
            let fds = super::stdin_fence_tests::pipe();
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
        ) -> (ReplyGate, Option<(crate::mcp::wire::Frame, u64, Instant)>) {
            self.gate_until(frames, Instant::now() + RELAY_REPLY_BOUND)
        }
        /// As `gate_on`, settling only until `bound`.
        fn gate_until(
            &self,
            frames: &FrameReceiver,
            bound: Instant,
        ) -> (ReplyGate, Option<(crate::mcp::wire::Frame, u64, Instant)>) {
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
                cancels: Vec::new(),
                outstanding: Vec::new(),
            };
            let mut stdout = Vec::new();
            let mut eof = false;
            let gate = reply_gate(
                frames,
                &mut buffer,
                &mut stdout,
                &mut eof,
                Some(&json!("A")),
                bound,
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
            if self.fds[1] >= 0 {
                unsafe { libc::close(self.fds[1]) };
            }
            let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
            while !self.fence.closed.load(Ordering::SeqCst) && Instant::now() < deadline {
                while self.frames.try_recv().is_ok() {}
                thread::sleep(Duration::from_millis(1));
            }
            unsafe { libc::close(self.fds[0]) };
        }
    }
    const CANCEL_A: &str = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":\"A\"}}\n";

    /// Decision 0009 §7: a call's settling ends by its own deadline. Input
    /// the stdin reader has not delivered by then makes the outcome
    /// outcome_unknown at that bound, not a later decision.
    #[test]
    fn settling_ends_by_the_calls_bound() {
        use std::sync::atomic::Ordering::SeqCst;
        let fixture = Fixture::new();
        // A complete frame the reader is held from delivering.
        fixture.fence.hold_reads.store(true, SeqCst);
        fixture.write("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n");
        let bound = Instant::now() + Duration::from_millis(200);
        // Released only well after the bound, or once the gate decided, so a
        // gate that settles past its bound would see the frame and write.
        let (decided_tx, decided_rx) = std::sync::mpsc::channel::<()>();
        let fence = fixture.fence.clone();
        let releaser = thread::spawn(move || {
            let _ = decided_rx.recv_timeout(Duration::from_secs(3));
            fence.hold_reads.store(false, SeqCst);
        });
        let (gate, _) = fixture.gate_until(&fixture.frames, bound);
        let _ = decided_tx.send(());
        releaser.join().unwrap();
        assert!(
            matches!(gate, ReplyGate::Unknown),
            "the gate settled past its bound"
        );
    }

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

    /// Block until the read end reports that no writer remains.
    fn wait_for_hangup(fd: libc::c_int) {
        let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
        loop {
            let mut ready = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut ready, 1, 0) } > 0 && ready.revents & libc::POLLHUP != 0 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the closed write end never hung up"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn stdin_eof_before_the_decision_cancels_the_reply() {
        for _ in 0..50 {
            let mut fixture = Fixture::new();
            // The client closes its input before the reply exists.
            unsafe { libc::close(fixture.fds[1]) };
            fixture.fds[1] = -1;
            wait_for_hangup(fixture.fds[0]);
            assert!(matches!(fixture.gate(), ReplyGate::Cancelled));
        }
    }

    #[test]
    fn stdin_eof_behind_buffered_frames_before_the_decision_cancels_the_reply() {
        use std::sync::atomic::Ordering::SeqCst;
        for _ in 0..50 {
            let mut fixture = Fixture::new();
            // Hold the reader so the frame is provably still buffered when the
            // decision is sampled, then close: all before the reply exists.
            fixture.fence.hold_reads.store(true, SeqCst);
            fixture.write("{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n");
            unsafe { libc::close(fixture.fds[1]) };
            fixture.fds[1] = -1;
            wait_for_hangup(fixture.fds[0]);
            assert!(
                fixture.fence.pending().unwrap() > 0,
                "precondition: bytes buffered"
            );
            let fence = fixture.fence.clone();
            *fixture.fence.after_sample.lock().unwrap() = Some(Box::new(move || {
                assert!(
                    fence.pending().unwrap() > 0,
                    "the sample saw buffered bytes"
                );
                fence.hold_reads.store(false, SeqCst);
            }));
            assert!(matches!(fixture.gate(), ReplyGate::Cancelled));
        }
    }

    #[test]
    fn a_write_and_close_between_the_two_measurements_counts_before_the_decision() {
        use std::sync::atomic::Ordering::SeqCst;
        for _ in 0..50 {
            let mut fixture = Fixture::new();
            fixture.fence.hold_reads.store(true, SeqCst);
            // After pending bytes are measured and before hang-up is checked,
            // the client writes a cancel for the active request and closes.
            let (fd, read_fd) = (fixture.fds[1], fixture.fds[0]);
            *fixture.fence.between_measures.lock().unwrap() = Some(Box::new(move || {
                assert_eq!(
                    unsafe { libc::write(fd, CANCEL_A.as_ptr().cast(), CANCEL_A.len()) },
                    CANCEL_A.len() as isize
                );
                unsafe { libc::close(fd) };
                // A process forked by a concurrent test can hold a copy of the
                // write end until it execs; the close is only complete once
                // the read end hangs up.
                wait_for_hangup(read_fd);
            }));
            let fence = fixture.fence.clone();
            *fixture.fence.after_sample.lock().unwrap() = Some(Box::new(move || {
                fence.hold_reads.store(false, SeqCst);
            }));
            let gate = fixture.gate();
            fixture.fds[1] = -1;
            assert!(matches!(gate, ReplyGate::Cancelled));
        }
    }

    #[test]
    fn a_failed_hangup_check_is_an_unknown_outcome() {
        let fixture = Fixture::new();
        fixture.write(CANCEL_A);
        fixture
            .fence
            .fail_poll
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(matches!(fixture.gate(), ReplyGate::Unknown));
    }

    #[test]
    fn stdin_eof_just_after_the_decision_cannot_suppress_the_reply() {
        for _ in 0..50 {
            let mut fixture = Fixture::new();
            // Right after the decision is sampled the client closes its input,
            // and the reader delivers that EOF before the gate drains.
            let fd = fixture.fds[1];
            let fence = fixture.fence.clone();
            *fixture.fence.after_sample.lock().unwrap() = Some(Box::new(move || {
                unsafe { libc::close(fd) };
                let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
                while !fence.closed.load(std::sync::atomic::Ordering::SeqCst) {
                    assert!(Instant::now() < deadline, "EOF never delivered");
                    thread::sleep(Duration::from_millis(1));
                }
            }));
            let (gate, deferred) = fixture.gate_on(&fixture.frames);
            fixture.fds[1] = -1;
            assert!(matches!(gate, ReplyGate::Write));
            assert!(
                matches!(deferred, Some((crate::mcp::wire::Frame::Eof, _, _))),
                "the post-decision EOF is deferred until after the reply"
            );
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
                let deadline = Instant::now() + crate::testing::HANG_BACKSTOP;
                while !fence.settled(through) {
                    assert!(Instant::now() < deadline, "later frame never delivered");
                    thread::sleep(Duration::from_millis(1));
                }
            }));
            let (gate, deferred) = fixture.gate_on(&fixture.frames);
            assert!(matches!(gate, ReplyGate::Write));
            match deferred {
                Some((crate::mcp::wire::Frame::Line(bytes), end, _)) => {
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
                        .send((
                            crate::mcp::wire::Frame::Line(line.clone()),
                            u64::MAX,
                            Instant::now(),
                        ))
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
        first.recv_timeout(crate::testing::HANG_BACKSTOP).unwrap();
        let (done, decided) = std::sync::mpsc::channel();
        let gate = thread::spawn(move || {
            let fixture = Fixture::new();
            let (gate, deferred) = fixture.gate_on(&frames);
            let _ = done.send(matches!(gate, ReplyGate::Write) && deferred.is_some());
        });
        let outcome = decided.recv_timeout(crate::testing::HANG_BACKSTOP);
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
