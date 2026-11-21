//! Bounded, nonblocking clipboard I/O; no waiting is allowed on the display loop.
use std::io::{self, ErrorKind, Read};
use std::os::fd::AsFd;
use std::task::Poll;
use std::time::{Duration, Instant};

pub(crate) const TRANSFER_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const IO_BUDGET: usize = 256 * 1024;
const MAX_TRANSFER_BYTES: usize = 256 * 1024 * 1024;

pub(crate) fn nonblocking(fd: &impl AsFd) -> io::Result<()> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
    let flags = fcntl_getfl(fd)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK)?;
    Ok(())
}

pub(crate) struct ReadTransfer<R> {
    reader: R,
    data: Vec<u8>,
    deadline: Instant,
}

impl<R: Read + AsFd> ReadTransfer<R> {
    pub(crate) fn new(reader: R, now: Instant) -> io::Result<Self> {
        nonblocking(&reader)?;
        Ok(Self {
            reader,
            data: Vec::new(),
            deadline: now + TRANSFER_TIMEOUT,
        })
    }

    pub(crate) fn advance(&mut self, now: Instant) -> Poll<io::Result<Vec<u8>>> {
        if now >= self.deadline {
            self.data.clear();
            return Poll::Ready(Err(ErrorKind::TimedOut.into()));
        }
        let mut buffer = [0; 16 * 1024];
        // Limit attempts as well as bytes, including repeated EINTR, to yield to
        // the display loop even if a producer writes continuously.
        for _ in 0..IO_BUDGET / buffer.len() {
            match self.reader.read(&mut buffer) {
                Ok(0) => return Poll::Ready(Ok(std::mem::take(&mut self.data))),
                Ok(n) => {
                    if self.data.len() + n > MAX_TRANSFER_BYTES {
                        return Poll::Ready(Err(ErrorKind::OutOfMemory.into()));
                    }
                    self.data.extend_from_slice(&buffer[..n]);
                    self.deadline = now + TRANSFER_TIMEOUT;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    fn pipe() -> (ReadTransfer<File>, File, Instant) {
        let (rx, tx) = rustix::pipe::pipe().unwrap();
        let now = Instant::now();
        (
            ReadTransfer::new(File::from(rx), now).unwrap(),
            File::from(tx),
            now,
        )
    }

    #[test]
    fn idle_open_writer_returns_immediately_and_times_out() {
        let (mut transfer, _writer, now) = pipe();
        for _ in 0..10 {
            assert!(transfer.advance(now).is_pending());
        }
        assert!(matches!(transfer.advance(now + TRANSFER_TIMEOUT),
            Poll::Ready(Err(e)) if e.kind() == ErrorKind::TimedOut));
    }

    #[test]
    fn partial_data_is_not_success_without_eof() {
        let (mut transfer, mut writer, now) = pipe();
        writer.write_all(b"incomplete").unwrap();
        assert!(transfer.advance(now).is_pending());
        assert!(matches!(transfer.advance(now + TRANSFER_TIMEOUT),
            Poll::Ready(Err(e)) if e.kind() == ErrorKind::TimedOut));
        assert!(transfer.data.is_empty());
    }

    #[test]
    fn progress_renews_idle_deadline() {
        let (mut transfer, mut writer, now) = pipe();
        writer.write_all(b"one").unwrap();
        assert!(transfer.advance(now + Duration::from_secs(9)).is_pending());
        assert!(transfer.advance(now + Duration::from_secs(15)).is_pending());
        writer.write_all(b"two").unwrap();
        assert!(transfer.advance(now + Duration::from_secs(18)).is_pending());
        drop(writer);
        assert!(matches!(transfer.advance(now + Duration::from_secs(25)),
            Poll::Ready(Ok(bytes)) if bytes == b"onetwo"));
    }

    #[test]
    fn transfer_larger_than_pipe_capacity_preserves_all_bytes() {
        let (mut transfer, mut writer, now) = pipe();
        nonblocking(&writer).unwrap();
        let expected: Vec<u8> = (0..1024 * 1024).map(|n| n as u8).collect();
        let mut offset = 0;
        while offset < expected.len() {
            match writer.write(&expected[offset..]) {
                Ok(n) => offset += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => panic!("{e}"),
            }
            assert!(transfer.advance(now).is_pending());
        }
        drop(writer);
        loop {
            match transfer.advance(now) {
                Poll::Ready(Ok(bytes)) => {
                    assert_eq!(bytes, expected);
                    break;
                }
                Poll::Ready(Err(e)) => panic!("{e}"),
                Poll::Pending => {}
            }
        }
    }
}
