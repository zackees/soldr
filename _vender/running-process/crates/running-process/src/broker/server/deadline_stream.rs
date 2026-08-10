//! Shared deadline-bounded I/O for accepted broker control connections.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

const CONTROL_IO_POLL_INTERVAL: Duration = Duration::from_millis(5);
const DEFAULT_HELLO_READ_TIMEOUT: Duration = Duration::from_secs(30);
const HELLO_READ_TIMEOUT_ENV: &str = "RUNNING_PROCESS_BROKER_HELLO_TIMEOUT_MS";

#[doc(hidden)]
pub fn hello_read_deadline() -> Instant {
    let timeout = std::env::var(HELLO_READ_TIMEOUT_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&ms| ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_HELLO_READ_TIMEOUT);
    Instant::now() + timeout
}

/// Stream capability required by [`with_nonblocking_deadline`].
#[doc(hidden)]
pub trait SetNonblocking {
    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()>;
}

impl SetNonblocking for interprocess::local_socket::Stream {
    fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        use interprocess::local_socket::traits::Stream as InterprocessStream;

        InterprocessStream::set_nonblocking(self, nonblocking)
    }
}

/// Run one operation with nonblocking I/O bounded by an absolute deadline.
///
/// Accepted broker streams are blocking by default. This helper restores that
/// mode on every success/error path; an operation error takes precedence over
/// a restoration error so timeout classification is not lost.
#[doc(hidden)]
pub fn with_nonblocking_deadline<S, T, E>(
    stream: &mut S,
    deadline: Instant,
    operation: impl FnOnce(&mut DeadlineStream<'_, S>) -> Result<T, E>,
) -> Result<T, E>
where
    S: SetNonblocking,
    E: From<std::io::Error>,
{
    stream.set_nonblocking(true).map_err(E::from)?;
    let result = {
        let mut deadline_stream = DeadlineStream::new(stream, deadline);
        operation(&mut deadline_stream)
    };
    let restored = stream.set_nonblocking(false).map_err(E::from);
    match result {
        Err(error) => Err(error),
        Ok(value) => {
            restored?;
            Ok(value)
        }
    }
}

/// Wraps a nonblocking broker stream and bounds I/O against a deadline.
#[doc(hidden)]
pub struct DeadlineStream<'a, S> {
    inner: &'a mut S,
    deadline: Instant,
}

impl<'a, S> DeadlineStream<'a, S> {
    pub fn new(inner: &'a mut S, deadline: Instant) -> Self {
        Self { inner, deadline }
    }

    fn wait(&self) -> std::io::Result<()> {
        self.ensure_not_expired()?;
        let now = Instant::now();
        std::thread::sleep((self.deadline - now).min(CONTROL_IO_POLL_INTERVAL));
        Ok(())
    }

    fn ensure_not_expired(&self) -> std::io::Result<()> {
        let now = Instant::now();
        if now >= self.deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for broker peer to send a complete frame",
            ));
        }
        Ok(())
    }
}

impl<S: Read> Read for DeadlineStream<'_, S> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            self.ensure_not_expired()?;
            match self.inner.read(buf) {
                // Windows `PIPE_NOWAIT` reports an empty pipe as `Ok(0)`;
                // Unix only returns zero for EOF, which framing must keep
                // distinct from a deadline expiry.
                Ok(0) if cfg!(windows) => self.wait()?,
                Ok(0) => return Ok(0),
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.wait()?,
                Err(error) if error.kind() == std::io::ErrorKind::TimedOut => self.wait()?,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

impl<S: Write> Write for DeadlineStream<'_, S> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        loop {
            self.ensure_not_expired()?;
            match self.inner.write(buf) {
                Ok(0) => self.wait()?,
                Ok(n) => return Ok(n),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.wait()?,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        loop {
            self.ensure_not_expired()?;
            match self.inner.flush() {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => self.wait()?,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io;

    #[cfg(unix)]
    #[test]
    fn eof_remains_distinct_from_timeout() {
        let mut empty = std::io::Cursor::new(Vec::<u8>::new());
        let mut stream = DeadlineStream::new(&mut empty, Instant::now() + Duration::from_secs(1));
        let mut byte = [0u8; 1];
        assert_eq!(stream.read(&mut byte).expect("EOF is not an error"), 0);
    }

    struct Silent;

    impl Read for Silent {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    #[test]
    fn silent_reader_times_out() {
        let mut silent = Silent;
        let mut stream =
            DeadlineStream::new(&mut silent, Instant::now() + Duration::from_millis(20));
        let error = stream.read(&mut [0_u8; 1]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    struct PartialThenSilent {
        supplied: bool,
    }

    impl Read for PartialThenSilent {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if !self.supplied {
                self.supplied = true;
                buf[0] = 1;
                return Ok(1);
            }
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    #[test]
    fn partial_reader_times_out_before_completion() {
        let mut partial = PartialThenSilent { supplied: false };
        let mut stream =
            DeadlineStream::new(&mut partial, Instant::now() + Duration::from_millis(20));
        let error = stream.read_exact(&mut [0_u8; 2]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    struct Trickle;

    impl Read for Trickle {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(8));
            buf[0] = 1;
            Ok(1)
        }
    }

    #[test]
    fn absolute_deadline_stops_continuous_trickle() {
        let mut trickle = Trickle;
        let mut stream =
            DeadlineStream::new(&mut trickle, Instant::now() + Duration::from_millis(20));
        let error = stream.read_exact(&mut [0_u8; 16]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[derive(Default)]
    struct ModeTrackingStream {
        mode_changes: RefCell<Vec<bool>>,
    }

    impl SetNonblocking for ModeTrackingStream {
        fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
            self.mode_changes.borrow_mut().push(nonblocking);
            Ok(())
        }
    }

    #[test]
    fn operation_error_is_preserved_and_blocking_mode_is_restored() {
        let mut stream = ModeTrackingStream::default();
        let result = with_nonblocking_deadline(
            &mut stream,
            Instant::now() + Duration::from_secs(1),
            |_stream| Err::<(), _>(io::Error::from(io::ErrorKind::TimedOut)),
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert_eq!(*stream.mode_changes.borrow(), [true, false]);
    }
}
