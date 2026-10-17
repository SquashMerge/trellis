//! Bounded, length-delimited socket messages. IDs identify responses after reconnects.
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Instant;

pub const MAX_FRAME: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    pub id: u64,
    pub operation: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reply {
    pub id: u64,
    pub payload: serde_json::Value,
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, message: &T) -> io::Result<()> {
    let bytes = serde_json::to_vec(message).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon frame too large",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub fn read_frame<T: for<'a> Deserialize<'a>>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0; 4];
    reader.read_exact(&mut length)?;
    let size = u32::from_be_bytes(length) as usize;
    if size > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "daemon frame too large",
        ));
    }
    let mut bytes = vec![0; size];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Metrics uses a single absolute budget for the entire frame, including a
/// peer that makes progress one byte at a time.
fn remaining(deadline: Instant) -> io::Result<std::time::Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "metrics frame deadline exceeded"))
}

struct DeadlineReader<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl Read for DeadlineReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            // On macOS SO_RCVTIMEO can reject an already-closed peer even
            // while its final complete frames remain buffered. A nonblocking
            // read still receives those bytes without exceeding this budget.
            let budget = remaining(self.deadline)?.min(std::time::Duration::from_millis(500));
            if let Err(error) = self.stream.set_read_timeout(Some(budget)) {
                if error.raw_os_error() != Some(libc::EINVAL) {
                    return Err(error);
                }
                self.stream.set_nonblocking(true)?;
                let result = self.stream.read(bytes);
                self.stream.set_nonblocking(false)?;
                let count = result?;
                remaining(self.deadline)?;
                return Ok(count);
            }
            match self.stream.read(bytes) {
                Ok(count) => {
                    remaining(self.deadline)?;
                    return Ok(count);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    remaining(self.deadline)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

struct DeadlineWriter<'a> {
    stream: &'a mut UnixStream,
    deadline: Instant,
}
impl Write for DeadlineWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.stream.set_write_timeout(Some(
                remaining(self.deadline)?.min(std::time::Duration::from_millis(500)),
            ))?;
            match self.stream.write(bytes) {
                Ok(count) => {
                    remaining(self.deadline)?;
                    return Ok(count);
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                    ) =>
                {
                    remaining(self.deadline)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.set_write_timeout(Some(
            remaining(self.deadline)?.min(std::time::Duration::from_millis(500)),
        ))?;
        self.stream.flush()?;
        remaining(self.deadline)?;
        Ok(())
    }
}

pub fn read_frame_until<T: for<'a> Deserialize<'a>>(
    stream: &mut UnixStream,
    deadline: Instant,
) -> io::Result<T> {
    let result = read_frame(&mut DeadlineReader { stream, deadline })?;
    remaining(deadline)?;
    Ok(result)
}

pub fn write_frame_until<T: Serialize>(
    stream: &mut UnixStream,
    message: &T,
    deadline: Instant,
) -> io::Result<()> {
    struct CheckedBytes {
        bytes: Vec<u8>,
        deadline: Instant,
    }
    impl Write for CheckedBytes {
        fn write(&mut self, part: &[u8]) -> io::Result<usize> {
            remaining(self.deadline)?;
            if part.len() > MAX_FRAME.saturating_sub(self.bytes.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "daemon frame too large",
                ));
            }
            self.bytes.extend_from_slice(part);
            remaining(self.deadline)?;
            Ok(part.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            remaining(self.deadline).map(|_| ())
        }
    }
    let mut serialized = CheckedBytes {
        bytes: Vec::new(),
        deadline,
    };
    serde_json::to_writer(&mut serialized, message).map_err(io::Error::other)?;
    remaining(deadline)?;
    let mut writer = DeadlineWriter { stream, deadline };
    writer.write_all(&(serialized.bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&serialized.bytes)?;
    writer.flush()?;
    remaining(deadline)?;
    Ok(())
}

pub fn write_preface_until(stream: &mut UnixStream, deadline: Instant) -> io::Result<()> {
    DeadlineWriter { stream, deadline }.write_all(&[0xff])?;
    remaining(deadline)?;
    Ok(())
}

/// The daemon has already consumed the reserved preface. The following four
/// bytes are the ordinary length prefix, with no change to the frame format.
pub fn read_prefaced_frame_until<T: for<'a> Deserialize<'a>>(
    stream: &mut UnixStream,
    deadline: Instant,
) -> io::Result<T> {
    read_frame_until(stream, deadline)
}
