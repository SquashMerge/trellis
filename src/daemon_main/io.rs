//! Reading daemon requests, writing replies and waiting for a `serve` client to disconnect.
#[cfg(feature = "test-support")]
use crate::metrics_test_duration;
use anyhow::Result;
use std::time::{Duration, Instant};
use trellis::daemon::protocol;

/// Reads one request from an accepted connection, and whether it was
/// prefaced (metrics admission). Each read also ends, as at end of input, on
/// the daemon's published shutdown, so the stop sweep never relies on a
/// cross-thread socket shutdown to end an incomplete request (#741).
pub(crate) fn read_daemon_request(
    stream: &std::os::unix::net::UnixStream,
    shutdown: tokio::sync::watch::Receiver<bool>,
    ingress_deadline: Instant,
    #[cfg(test)] waiting: Option<trellis::daemon::shutdown_read::WaitHookForTests>,
) -> Result<(protocol::Request, bool)> {
    use std::io::{Cursor, Read};
    let mut reader =
        trellis::daemon::shutdown_read::ShutdownReader::new(stream.try_clone()?, shutdown)?;
    #[cfg(test)]
    if let Some(waiting) = waiting {
        reader = reader.on_wait_for_tests(waiting);
    }
    reader.set_idle_timeout(Some(Duration::from_secs(15)));
    let mut first = [0u8; 1];
    reader.read_exact(&mut first)?;
    let prefaced = first[0] == 0xff;
    let request = if prefaced {
        // The reserved preface is consumed. The frame that follows has one
        // absolute budget, including a peer that sends one byte at a time.
        reader.set_idle_timeout(None);
        reader.set_deadline(Some(ingress_deadline));
        protocol::read_frame(&mut reader)?
    } else {
        protocol::read_frame(&mut Cursor::new(first).chain(&mut reader))?
    };
    Ok((request, prefaced))
}

pub(crate) fn wait_for_serve_disconnect(
    stream: std::os::unix::net::UnixStream,
    shutdown: tokio::sync::watch::Receiver<bool>,
    runtime: &tokio::runtime::Handle,
) -> Result<()> {
    wait_for_serve_disconnect_hooked(
        stream,
        shutdown,
        runtime,
        #[cfg(test)]
        None,
    )
}

/// Holds the serve registration until the peer closes the control connection
/// or the daemon publishes shutdown. Both are awaited in one runtime task: on
/// Darwin, `shutdown(2)` on a cloned descriptor does not reliably wake a
/// thread entering a blocking `read(2)` on the same socket (#739), so the
/// shutdown signal must end this wait directly rather than through the socket.
pub(crate) fn wait_for_serve_disconnect_hooked(
    stream: std::os::unix::net::UnixStream,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
    runtime: &tokio::runtime::Handle,
    #[cfg(test)] waiting: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    // fcntl(O_NONBLOCK) also succeeds on a socket the daemon already shut down
    // (#625), unlike SO_RCVTIMEO on Darwin.
    stream.set_nonblocking(true)?;
    let waiter = runtime.spawn(async move {
        // Registration needs the runtime's reactor, so it happens on the task.
        let stream = tokio::net::UnixStream::from_std(stream)?;
        #[cfg(test)]
        if let Some(waiting) = waiting {
            let _ = waiting.send(());
        }
        let mut one = [0u8; 1];
        loop {
            tokio::select! {
                // A dropped sender also means the daemon is stopping.
                _ = shutdown.wait_for(|stopped| *stopped) => return Ok::<(), std::io::Error>(()),
                ready = stream.readable() => {
                    if ready.is_err() {
                        return Ok(());
                    }
                    match stream.try_read(&mut one) {
                        Ok(n) if n > 0 => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                        // EOF or a read error ends the session, as before.
                        _ => return Ok(()),
                    }
                }
            }
        }
    });
    match runtime.block_on(waiter) {
        Ok(result) => Ok(result?),
        // The runtime is shutting down, which also ends the session.
        Err(error) if error.is_cancelled() => Ok(()),
        Err(error) => Err(anyhow::anyhow!("serve control wait failed: {error}")),
    }
}

pub(super) fn write_daemon_reply(
    stream: &mut std::os::unix::net::UnixStream,
    reply: &protocol::Reply,
) -> Result<()> {
    let payload = serde_json::to_vec(&reply.payload)?;
    if payload.len() + 64 <= protocol::MAX_FRAME {
        protocol::write_frame(stream, reply)?;
    } else {
        const CHUNK_BYTES: usize = 450_000;
        for (index, chunk) in payload.chunks(CHUNK_BYTES).enumerate() {
            let more = (index + 1) * CHUNK_BYTES < payload.len();
            protocol::write_frame(
                stream,
                &protocol::Reply {
                    id: reply.id,
                    payload: serde_json::json!({"chunk": hex::encode(chunk), "more": more}),
                },
            )?;
        }
    }
    Ok(())
}

pub(super) fn result_is_deadline(payload: &serde_json::Value) -> bool {
    payload["error"]
        .as_str()
        .is_some_and(|value| value.contains("deadline_exceeded") || value.contains("cancelled"))
}

pub(super) fn write_daemon_reply_until(
    stream: &mut std::os::unix::net::UnixStream,
    reply: &protocol::Reply,
    deadline: Instant,
) -> Result<()> {
    // The selected response was fully materialized and fenced before transfer.
    struct TransferBytes {
        bytes: Vec<u8>,
        deadline: Instant,
    }
    impl std::io::Write for TransferBytes {
        fn write(&mut self, part: &[u8]) -> std::io::Result<usize> {
            if Instant::now() >= self.deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "metrics transfer deadline exceeded",
                ));
            }
            if part.len() > (64 * 1024 * 1024 + 4096_usize).saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "metrics transfer too large",
                ));
            }
            self.bytes.extend_from_slice(part);
            if Instant::now() >= self.deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "metrics transfer deadline exceeded",
                ));
            }
            Ok(part.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut serialized = TransferBytes {
        bytes: Vec::new(),
        deadline,
    };
    serde_json::to_writer(&mut serialized, &reply.payload)?;
    let payload = serialized.bytes;
    if Instant::now() >= deadline {
        anyhow::bail!("deadline_exceeded");
    }
    const CHUNK_BYTES: usize = 450_000;
    let chunk_bytes = CHUNK_BYTES;
    #[cfg(feature = "test-support")]
    let forced_chunks = std::env::var("TRELLIS_TEST_METRICS_FORCE_CHUNK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| (1..=CHUNK_BYTES).contains(value));
    #[cfg(feature = "test-support")]
    let chunk_bytes = forced_chunks.unwrap_or(chunk_bytes);
    #[cfg(feature = "test-support")]
    let last_chunk_delay = metrics_test_duration("TRELLIS_TEST_METRICS_LAST_CHUNK_DELAY_MS");
    #[cfg(feature = "test-support")]
    let small_frame = forced_chunks.is_none() && payload.len() + 64 <= protocol::MAX_FRAME;
    #[cfg(not(feature = "test-support"))]
    let small_frame = payload.len() + 64 <= protocol::MAX_FRAME;
    if small_frame {
        protocol::write_frame_until(stream, reply, deadline)?;
    } else {
        for (index, chunk) in payload.chunks(chunk_bytes).enumerate() {
            let more = (index + 1) * chunk_bytes < payload.len();
            #[cfg(feature = "test-support")]
            if !more && let Some(delay) = last_chunk_delay {
                std::thread::sleep(delay.min(
                    deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(100),
                ));
            }
            protocol::write_frame_until(
                stream,
                &protocol::Reply {
                    id: reply.id,
                    payload: serde_json::json!({"chunk": hex::encode(chunk), "more": more}),
                },
                deadline,
            )?;
            #[cfg(feature = "test-support")]
            if let Ok(path) = std::env::var("TRELLIS_TEST_METRICS_CHUNK_COUNT_FILE") {
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                {
                    let _ = file.write_all(b"x");
                }
            }
        }
    }
    Ok(())
}
