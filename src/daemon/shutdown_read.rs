//! Daemon connection reads that also end on the daemon's published shutdown.
//!
//! On Darwin, `shutdown(2)` on a cloned descriptor can return without waking a
//! thread that is entering a blocking `read(2)` on the same socket (#739), so
//! the stop sweep cannot rely on it to end a connection read (#741). Each read
//! here waits on the owner-published shutdown signal and the socket's
//! readability together, in one future, and reports a published shutdown as
//! end of input, which is what a socket shutdown produced before.
//!
//! Each reader drives that future on a small runtime of its own, so a read
//! never depends on another thread driving the daemon runtime's reactor. The
//! socket is never switched to nonblocking mode, because its open file
//! description is shared with the connection's blocking writer; each receive
//! is made nonblocking with `MSG_DONTWAIT` instead.
use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;

/// Called each time a read has found no input and is about to wait.
pub type WaitHookForTests = Box<dyn FnMut() + Send>;

/// A blocking reader for one daemon connection. Read and drop it on a thread
/// that may block, never inside an async task.
pub struct ShutdownReader {
    // Dropped before the runtime it is registered with.
    socket: AsyncFd<UnixStream>,
    runtime: tokio::runtime::Runtime,
    shutdown: tokio::sync::watch::Receiver<bool>,
    idle: Option<Duration>,
    deadline: Option<Instant>,
    waiting: Option<WaitHookForTests>,
}

impl ShutdownReader {
    /// Reads from `socket`, a descriptor of its own for one connection, until
    /// `shutdown` holds `true` or its sender is dropped.
    pub fn new(
        socket: UnixStream,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        let socket = {
            // Registration needs the reader runtime's reactor.
            let _context = runtime.enter();
            // SAFETY: the `AsyncFd` owns `socket`, whose descriptor stays open
            // and refers to the same socket until the `AsyncFd` drops it.
            unsafe { AsyncFd::register_with_interest(socket, Interest::READABLE) }?
        };
        Ok(Self {
            socket,
            runtime,
            shutdown,
            idle: None,
            deadline: None,
            waiting: None,
        })
    }

    /// Each read fails with `TimedOut` after waiting this long for input.
    pub fn set_idle_timeout(&mut self, idle: Option<Duration>) {
        self.idle = idle;
    }

    /// One absolute budget for every read, including a peer that makes
    /// progress one byte at a time. A read that completes after it fails.
    pub fn set_deadline(&mut self, deadline: Option<Instant>) {
        self.deadline = deadline;
    }

    #[doc(hidden)]
    pub fn on_wait_for_tests(mut self, hook: WaitHookForTests) -> Self {
        self.waiting = Some(hook);
        self
    }

    fn receive(socket: &UnixStream, bytes: &mut [u8]) -> io::Result<usize> {
        // SAFETY: `bytes` is a valid writable buffer of the given length and
        // the descriptor stays open for the duration of the call.
        let count = unsafe {
            libc::recv(
                socket.as_raw_fd(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if count < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }

    fn check_deadline(&self) -> io::Result<()> {
        match self.deadline {
            Some(deadline) if Instant::now() >= deadline => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "daemon frame deadline exceeded",
            )),
            _ => Ok(()),
        }
    }

    fn stopped(&self) -> bool {
        // A dropped sender also means the daemon is stopping.
        *self.shutdown.borrow() || self.shutdown.has_changed().is_err()
    }
}

impl io::Read for ShutdownReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.stopped() {
            return Ok(0);
        }
        self.check_deadline()?;
        loop {
            match Self::receive(self.socket.get_ref(), bytes) {
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Ok(count) => {
                    self.check_deadline()?;
                    return Ok(count);
                }
                Err(error) => return Err(error),
            }
        }
        if let Some(waiting) = &mut self.waiting {
            waiting();
        }
        let expiry = [self.idle.map(|idle| Instant::now() + idle), self.deadline]
            .into_iter()
            .flatten()
            .min();
        let Self {
            socket,
            shutdown,
            runtime,
            ..
        } = self;
        let read = runtime.block_on(async {
            let expired = async {
                match expiry {
                    Some(at) => tokio::time::sleep_until(at.into()).await,
                    None => std::future::pending().await,
                }
            };
            tokio::pin!(expired);
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown.wait_for(|stopped| *stopped) => return Ok(0),
                    () = &mut expired => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "daemon connection read timed out",
                        ));
                    }
                    ready = socket.readable() => {
                        let mut ready = ready?;
                        match ready.try_io(|socket| Self::receive(socket.get_ref(), bytes)) {
                            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
                            Ok(read) => return read,
                            // Readiness was stale and is now cleared.
                            Err(_would_block) => {}
                        }
                    }
                }
            }
        })?;
        if read > 0 {
            self.check_deadline()?;
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn parked_reader(
        socket: UnixStream,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> (ShutdownReader, std::sync::mpsc::Receiver<()>) {
        let (parked, waiting) = std::sync::mpsc::channel();
        let reader = ShutdownReader::new(socket, shutdown)
            .unwrap()
            .on_wait_for_tests(Box::new(move || {
                let _ = parked.send(());
            }));
        (reader, waiting)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reads_buffered_input_and_peer_end_of_input() {
        let (socket, peer) = UnixStream::pair().unwrap();
        let (_shutdown, stopped) = tokio::sync::watch::channel(false);
        let (mut reader, _waiting) = parked_reader(socket, stopped);
        let read = tokio::task::spawn_blocking(move || {
            let mut text = Vec::new();
            reader.read_to_end(&mut text).map(|_| text)
        });
        (&peer).write_all(b"frame").unwrap();
        drop(peer);
        let text = tokio::time::timeout(crate::testing::HANG_BACKSTOP, read)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(text, b"frame");
    }

    /// #741: the published signal alone ends a parked read while the peer
    /// stays open, and the socket still carries data afterwards, so no socket
    /// shutdown ended it.
    #[tokio::test(flavor = "current_thread")]
    async fn published_shutdown_ends_a_parked_read_without_a_socket_shutdown() {
        let (socket, peer) = UnixStream::pair().unwrap();
        let probe = socket.try_clone().unwrap();
        let (shutdown, stopped) = tokio::sync::watch::channel(false);
        let (mut reader, waiting) = parked_reader(socket, stopped);
        let read = tokio::task::spawn_blocking(move || reader.read(&mut [0; 8]));
        let parked = tokio::task::spawn_blocking(move || {
            waiting.recv_timeout(crate::testing::HANG_BACKSTOP)
        });
        parked.await.unwrap().expect("read never parked");
        shutdown.send_replace(true);
        let read = tokio::time::timeout(crate::testing::HANG_BACKSTOP, read)
            .await
            .expect("published shutdown did not end the parked read")
            .unwrap();
        assert_eq!(read.unwrap(), 0);
        (&peer).write_all(&[7]).unwrap();
        let mut byte = [0];
        (&probe).read_exact(&mut byte).unwrap();
        assert_eq!(byte, [7], "the read ended by a socket shutdown");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn an_idle_read_times_out_typed() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let (_shutdown, stopped) = tokio::sync::watch::channel(false);
        let mut reader = ShutdownReader::new(socket, stopped).unwrap();
        reader.set_idle_timeout(Some(Duration::from_millis(20)));
        let read = tokio::task::spawn_blocking(move || reader.read(&mut [0; 8]))
            .await
            .unwrap();
        assert_eq!(read.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
