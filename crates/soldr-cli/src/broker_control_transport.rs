//! User-facing daemon control traffic over the one stable broker listener.
//!
//! `soldr-daemon` retains a private control listener for its own lifecycle
//! implementation. The CLI installs this connector once at process startup,
//! so status, shutdown, cache metadata, build logs, GC, and session accounting
//! never derive or dial that daemon endpoint themselves.

#[cfg(unix)]
use interprocess::local_socket::traits::Stream as _;
use prost::Message as _;
use running_process::broker::protocol::{
    encode_framed, Frame, FrameKind, PayloadEncoding, ENVELOPE_VERSION, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};
use std::io::{self, Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::broker_server::{
    DaemonControlTunnelReply, DaemonControlTunnelRequest, DAEMON_CONTROL_PAYLOAD_PROTOCOL,
};

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) fn install() -> Result<(), &'static str> {
    crate::daemon::client::install_control_connector(Arc::new(BrokerControlConnector))
}

struct BrokerControlConnector;

impl crate::daemon::client::ControlConnector for BrokerControlConnector {
    fn connect(
        &self,
        _endpoint_marker: &Path,
        timeout: Duration,
    ) -> io::Result<crate::daemon::client::BoxedControlStream> {
        // Route derivation may hash a freshly-built daemon image. Complete it
        // before connecting so the broker's post-connect first-frame deadline
        // measures transport responsiveness, not client-side preparation.
        let service_name = crate::daemon::backend_handle_adoption::broker_service_name()?;
        let endpoint =
            crate::broker_identity::ResolvedBrokerEndpoint::resolve().map_err(io::Error::other)?;
        // A replacement broker performs one exact BackendHandle verification
        // before it can re-adopt a surviving daemon. That verification hashes
        // the executable and can exceed the ordinary 2s status reply budget
        // for a large debug image. The route handshake gets a separate bounded
        // allowance; once accepted, restore the caller's request timeout.
        let route_timeout = route_handshake_timeout(timeout);
        #[cfg(windows)]
        return connect_windows_control(
            endpoint.bind_endpoint,
            route_timeout,
            timeout,
            service_name,
        );
        #[cfg(unix)]
        {
            let name = crate::session_transport::local_session_name(&endpoint.bind_endpoint)?;
            let stream = interprocess::local_socket::ConnectOptions::new()
                .name(name)
                .wait_mode(interprocess::ConnectWaitMode::Timeout(route_timeout))
                .connect_sync()?;
            stream.set_recv_timeout(Some(route_timeout))?;
            stream.set_send_timeout(Some(route_timeout))?;
            let stream = negotiate_control_tunnel(stream, route_timeout, service_name)?;
            stream.set_recv_timeout(Some(timeout.max(Duration::from_millis(200))))?;
            stream.set_send_timeout(Some(timeout))?;
            Ok(Box::new(stream))
        }
    }
}

#[cfg(windows)]
struct WindowsDeadlineStream {
    commands: std::sync::mpsc::Sender<WindowsIoCommand>,
    recv_timeout: Duration,
    send_timeout: Duration,
}

#[cfg(windows)]
enum WindowsIoCommand {
    Read {
        len: usize,
        timeout: Duration,
        reply: std::sync::mpsc::Sender<io::Result<Vec<u8>>>,
    },
    Write {
        data: Vec<u8>,
        timeout: Duration,
        reply: std::sync::mpsc::Sender<io::Result<usize>>,
    },
    Flush {
        timeout: Duration,
        reply: std::sync::mpsc::Sender<io::Result<()>>,
    },
}

#[cfg(windows)]
impl WindowsDeadlineStream {
    fn spawn(
        endpoint: String,
        connect_timeout: Duration,
        recv_timeout: Duration,
        send_timeout: Duration,
    ) -> io::Result<Self> {
        let (commands, receiver) = std::sync::mpsc::channel();
        let (startup_tx, startup_rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("soldr-broker-control-io".into())
            .spawn(move || {
                windows_control_io_worker(endpoint, connect_timeout, receiver, startup_tx)
            })?;
        windows_control_reply(
            startup_rx,
            "connect",
            connect_timeout.saturating_add(Duration::from_secs(1)),
        )?;
        Ok(Self {
            commands,
            recv_timeout,
            send_timeout,
        })
    }

    fn send_command(&self, command: WindowsIoCommand) -> io::Result<()> {
        self.commands.send(command).map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "Windows broker control I/O worker stopped",
            )
        })
    }
}

#[cfg(windows)]
fn windows_control_io_worker(
    endpoint: String,
    connect_timeout: Duration,
    commands: std::sync::mpsc::Receiver<WindowsIoCommand>,
    startup: std::sync::mpsc::Sender<io::Result<()>>,
) {
    use interprocess::os::windows::named_pipe::{pipe_mode::Bytes, tokio::DuplexPipeStream};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = startup.send(Err(error));
            return;
        }
    };
    let mut stream =
        match runtime.block_on(DuplexPipeStream::<Bytes>::connect_by_path_with_wait_mode(
            endpoint.as_str(),
            interprocess::ConnectWaitMode::Timeout(connect_timeout),
        )) {
            Ok(stream) => stream,
            Err(error) => {
                let _ = startup.send(Err(error));
                return;
            }
        };
    if startup.send(Ok(())).is_err() {
        return;
    }

    while let Ok(command) = commands.recv() {
        match command {
            WindowsIoCommand::Read {
                len,
                timeout,
                reply,
            } => {
                let mut data = vec![0_u8; len];
                let result = runtime.block_on(async {
                    tokio::time::timeout(timeout, stream.read(&mut data))
                        .await
                        .map_err(|_| windows_control_timeout("read", timeout))?
                        .map(|read| {
                            data.truncate(read);
                            data
                        })
                });
                let _ = reply.send(result);
            }
            WindowsIoCommand::Write {
                data,
                timeout,
                reply,
            } => {
                let result = runtime.block_on(async {
                    tokio::time::timeout(timeout, stream.write(&data))
                        .await
                        .map_err(|_| windows_control_timeout("write", timeout))?
                });
                let _ = reply.send(result);
            }
            WindowsIoCommand::Flush { timeout, reply } => {
                let result = runtime.block_on(async {
                    tokio::time::timeout(timeout, stream.flush())
                        .await
                        .map_err(|_| windows_control_timeout("flush", timeout))?
                });
                let _ = reply.send(result);
            }
        }
    }
}

#[cfg(windows)]
fn windows_control_reply<T>(
    reply: std::sync::mpsc::Receiver<io::Result<T>>,
    operation: &str,
    timeout: Duration,
) -> io::Result<T> {
    match reply.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Err(windows_control_timeout(operation, timeout))
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "Windows broker control I/O worker stopped",
        )),
    }
}

#[cfg(windows)]
impl Read for WindowsDeadlineStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let timeout = self.recv_timeout;
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send_command(WindowsIoCommand::Read {
            len: buf.len(),
            timeout,
            reply: reply_tx,
        })?;
        let data = windows_control_reply(
            reply_rx,
            "read worker",
            timeout.saturating_add(Duration::from_secs(1)),
        )?;
        buf[..data.len()].copy_from_slice(&data);
        Ok(data.len())
    }
}

#[cfg(windows)]
impl Write for WindowsDeadlineStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        let timeout = self.send_timeout;
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send_command(WindowsIoCommand::Write {
            data: buf.to_vec(),
            timeout,
            reply: reply_tx,
        })?;
        windows_control_reply(
            reply_rx,
            "write worker",
            timeout.saturating_add(Duration::from_secs(1)),
        )
    }

    fn flush(&mut self) -> io::Result<()> {
        let timeout = self.send_timeout;
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send_command(WindowsIoCommand::Flush {
            timeout,
            reply: reply_tx,
        })?;
        windows_control_reply(
            reply_rx,
            "flush worker",
            timeout.saturating_add(Duration::from_secs(1)),
        )
    }
}

#[cfg(windows)]
fn connect_windows_control(
    endpoint: String,
    route_timeout: Duration,
    request_timeout: Duration,
    service_name: String,
) -> io::Result<crate::daemon::client::BoxedControlStream> {
    // Interprocess 2.4.3's high-level Windows local-socket adapters discard
    // ConnectWaitMode, so the worker uses its low-level named-pipe Tokio API
    // directly. That API preserves the bounded connect and supports
    // cancellation of each overlapped I/O future at its deadline. Keeping the
    // runtime on the worker also remains safe when synchronous daemon control
    // is called from inside Soldr's existing Tokio runtime.
    let stream =
        WindowsDeadlineStream::spawn(endpoint, route_timeout, route_timeout, route_timeout)?;
    let mut stream = negotiate_control_tunnel(stream, route_timeout, service_name)?;
    stream.recv_timeout = request_timeout.max(Duration::from_millis(200));
    stream.send_timeout = request_timeout;
    Ok(Box::new(stream))
}

#[cfg(windows)]
fn windows_control_timeout(operation: &str, timeout: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "Windows broker control {operation} timed out after {}ms",
            timeout.as_millis()
        ),
    )
}

fn route_handshake_timeout(request_timeout: Duration) -> Duration {
    if request_timeout <= Duration::from_millis(100) {
        request_timeout
    } else {
        request_timeout.max(Duration::from_secs(30))
    }
}

fn negotiate_control_tunnel<S: Read + Write>(
    mut stream: S,
    timeout: Duration,
    service_name: String,
) -> io::Result<S> {
    let frame = control_tunnel_frame(timeout, service_name);
    let request_id = frame.request_id;
    stream.write_all(&encode_framed(&frame).map_err(io::Error::other)?)?;
    stream.flush()?;

    validate_control_tunnel_reply(read_broker_frame(&mut stream)?, request_id)?;
    Ok(stream)
}

fn control_tunnel_frame(timeout: Duration, service_name: String) -> Frame {
    let host = running_process::broker::host_identity::current();
    let request = DaemonControlTunnelRequest {
        service_name,
        machine_id: host.machine_id,
        boot_id: host.boot_id,
    };
    let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed).max(1);
    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: DAEMON_CONTROL_PAYLOAD_PROTOCOL,
        payload: request.encode_to_vec(),
        request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: unix_deadline_ms(timeout),
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn validate_control_tunnel_reply(reply_frame: Frame, request_id: u64) -> io::Result<()> {
    if reply_frame.request_id != request_id
        || reply_frame.payload_protocol != DAEMON_CONTROL_PAYLOAD_PROTOCOL
        || FrameKind::try_from(reply_frame.kind) != Ok(FrameKind::Response)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker returned a mismatched daemon-control tunnel reply",
        ));
    }
    let reply = DaemonControlTunnelReply::decode(reply_frame.payload.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !reply.accepted {
        return Err(io::Error::new(
            if reply.not_running {
                io::ErrorKind::NotFound
            } else {
                io::ErrorKind::PermissionDenied
            },
            reply.error_detail,
        ));
    }
    Ok(())
}

fn read_broker_frame(stream: &mut impl Read) -> io::Result<Frame> {
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header)?;
    if header[0] != ENVELOPE_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported broker framing version",
        ));
    }
    let len = u32::from_le_bytes(header[1..].try_into().expect("four bytes")) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker frame exceeds the maximum size",
        ));
    }
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body)?;
    Frame::decode(body.as_slice())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn unix_deadline_ms(timeout: Duration) -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .saturating_add(timeout)
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    crate::timed_test!(control_tunnel_deadline_is_future_and_bounded, {
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let deadline = unix_deadline_ms(Duration::from_secs(2));
        assert!(deadline >= before + 1_900);
        assert!(deadline <= before + 2_100);
    });

    crate::timed_test!(hot_path_timeout_is_not_inflated_by_route_handshake, {
        assert_eq!(
            route_handshake_timeout(Duration::from_millis(40)),
            Duration::from_millis(40)
        );
        assert_eq!(
            route_handshake_timeout(Duration::from_secs(2)),
            Duration::from_secs(30)
        );
        assert_eq!(
            route_handshake_timeout(Duration::from_secs(45)),
            Duration::from_secs(45)
        );
    });

    #[cfg(windows)]
    crate::timed_test!(windows_missing_pipe_is_bounded_inside_tokio_runtime, {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let endpoint = format!(
                r"\\.\pipe\soldr-missing-control-test-{}-{}",
                std::process::id(),
                NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
            );
            let timeout = Duration::from_millis(100);
            let started = std::time::Instant::now();
            let result = WindowsDeadlineStream::spawn(endpoint, timeout, timeout, timeout);
            assert!(result.is_err());
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "missing named pipe exceeded its bounded connect: {:?}",
                started.elapsed()
            );
        });
    });
}
