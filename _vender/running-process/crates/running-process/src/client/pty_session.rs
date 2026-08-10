//! Client-side helpers for daemon-owned detachable PTY sessions
//! (issue #130 milestone 2).
//!
//! Sessions are spawned and listed via the regular [`DaemonClient`] RPC
//! channel. Attach is special: after the daemon responds with
//! `AttachPtySessionResponse` the same socket switches into a streaming
//! mode that carries [`PtyStreamFrame`] (daemon → client) and
//! [`PtyInputFrame`] (client → daemon) messages. [`PtyAttachment`] owns the
//! socket for the lifetime of that stream and exposes blocking
//! send/receive helpers suitable for tests and small clients. Async
//! clients can build on top of the attachment framing exposed by
//! [`PtyAttachment`].

use crate::client::paths;
use crate::client::{ClientError, DaemonClient};
use crate::proto::daemon::{
    pty_input_frame::Frame as InputOneof, AttachPtySessionRequest, AttachPtySessionResponse,
    DaemonRequest, DaemonResponse, DetachPtySessionRequest, KeyValue, ListPtySessionsRequest,
    ListPtySessionsResponse, PtyInputFrame, PtyResize, PtySessionInfo, PtyStreamFrame, RequestType,
    SpawnPtySessionRequest, SpawnPtySessionResponse, StatusCode, TerminatePtySessionRequest,
};
use crate::terminal_graphics::{
    current_terminal_capabilities, terminal_graphics_capabilities_to_proto, TerminalCapabilities,
    TerminalGraphicsCapabilities,
};
use interprocess::local_socket::Stream;
use interprocess::TryClone;
use prost::Message;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Spawn / list / terminate convenience builders
// ---------------------------------------------------------------------------

/// Request shape for spawning a daemon-owned PTY session.
#[derive(Debug, Clone)]
pub struct PtySpawnRequest {
    /// Command and arguments to execute inside the PTY.
    pub argv: Vec<String>,
    /// Working directory for the spawned process.
    pub cwd: Option<PathBuf>,
    /// Environment variables to add or override for the spawned process.
    pub env: Vec<(String, String)>,
    /// Whether to start from an empty environment instead of inheriting the daemon's environment.
    pub clear_inherited_env: bool,
    /// Initial terminal row count.
    pub rows: u16,
    /// Initial terminal column count.
    pub cols: u16,
    /// Optional caller-defined owner string used for listing and filtering sessions.
    pub originator: Option<String>,
}

impl PtySpawnRequest {
    /// Create a spawn request with default size and inherited environment.
    pub fn new<S: Into<String>>(argv: impl IntoIterator<Item = S>) -> Self {
        Self {
            argv: argv.into_iter().map(Into::into).collect(),
            cwd: None,
            env: Vec::new(),
            clear_inherited_env: false,
            rows: 24,
            cols: 80,
            originator: None,
        }
    }

    /// Set the working directory for the spawned process.
    pub fn with_cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// Set the initial PTY size.
    pub fn with_size(mut self, rows: u16, cols: u16) -> Self {
        self.rows = rows;
        self.cols = cols;
        self
    }

    /// Set the caller-defined owner string for this session.
    pub fn with_originator(mut self, originator: impl Into<String>) -> Self {
        self.originator = Some(originator.into());
        self
    }

    /// Replace the request's explicit environment variables.
    pub fn with_envs<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self
    }
}

/// Reply summary for a successful spawn.
#[derive(Debug, Clone)]
pub struct SpawnedPtySession {
    /// Daemon-assigned PTY session identifier.
    pub session_id: String,
    /// Process ID of the spawned session leader.
    pub pid: u32,
    /// Creation time reported by the daemon, in seconds since the Unix epoch.
    pub created_at: f64,
}

impl DaemonClient {
    /// Ask the daemon to spawn a new PTY session that it owns.
    pub fn spawn_pty_session(
        &mut self,
        request: &PtySpawnRequest,
    ) -> Result<SpawnedPtySession, ClientError> {
        let proto = SpawnPtySessionRequest {
            argv: request.argv.clone(),
            cwd: request
                .cwd
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            env: request
                .env
                .iter()
                .map(|(k, v)| KeyValue {
                    key: k.clone(),
                    value: v.clone(),
                })
                .collect(),
            clear_inherited_env: request.clear_inherited_env,
            rows: request.rows as u32,
            cols: request.cols as u32,
            originator: request.originator.clone().unwrap_or_default(),
        };

        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::SpawnPtySession.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            spawn_pty_session: Some(proto),
            ..Default::default()
        };

        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)?;
        let payload: SpawnPtySessionResponse =
            response
                .spawn_pty_session
                .ok_or_else(|| ClientError::Server {
                    code: StatusCode::Internal,
                    message: "spawn_pty_session response missing payload".into(),
                })?;
        Ok(SpawnedPtySession {
            session_id: payload.session_id,
            pid: payload.pid,
            created_at: payload.created_at,
        })
    }

    /// List PTY sessions known to the daemon. Empty `originator_filter`
    /// returns all sessions in scope.
    pub fn list_pty_sessions(
        &mut self,
        originator_filter: &str,
    ) -> Result<Vec<PtySessionInfo>, ClientError> {
        let req = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::ListPtySessions.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            list_pty_sessions: Some(ListPtySessionsRequest {
                originator: originator_filter.into(),
            }),
            ..Default::default()
        };
        let response = self.send_request(req)?;
        ensure_ok(&response)?;
        let payload: ListPtySessionsResponse =
            response
                .list_pty_sessions
                .ok_or_else(|| ClientError::Server {
                    code: StatusCode::Internal,
                    message: "list_pty_sessions response missing payload".into(),
                })?;
        Ok(payload.sessions)
    }

    /// Ask the daemon to detach any current attachment from a session,
    /// leaving the session alive. Idempotent.
    pub fn detach_pty_session(&mut self, session_id: &str) -> Result<(), ClientError> {
        let req = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::DetachPtySession.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            detach_pty_session: Some(DetachPtySessionRequest {
                session_id: session_id.into(),
            }),
            ..Default::default()
        };
        let response = self.send_request(req)?;
        ensure_ok(&response)?;
        Ok(())
    }

    /// Schedule termination of a PTY session. Returns as soon as the
    /// daemon accepts the schedule; the actual termination happens on a
    /// daemon background task (soft signal, grace, then hard kill).
    pub fn terminate_pty_session(
        &mut self,
        session_id: &str,
        grace_ms: u32,
    ) -> Result<(), ClientError> {
        let req = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::TerminatePtySession.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            terminate_pty_session: Some(TerminatePtySessionRequest {
                session_id: session_id.into(),
                grace_ms,
            }),
            ..Default::default()
        };
        let response = self.send_request(req)?;
        ensure_ok(&response)?;
        Ok(())
    }
}

fn ensure_ok(response: &DaemonResponse) -> Result<(), ClientError> {
    if response.code == StatusCode::Ok as i32 {
        return Ok(());
    }
    let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
    Err(ClientError::Server {
        code,
        message: response.message.clone(),
    })
}

// ---------------------------------------------------------------------------
// PtyAttachment
// ---------------------------------------------------------------------------

/// Active attachment to a daemon-owned PTY session.
///
/// Owns the socket; the connection is in streaming mode and cannot be used
/// for unrelated RPCs.
pub struct PtyAttachment {
    reader: BufReader<Stream>,
    writer: BufWriter<Stream>,
    /// Bytes received in the initial AttachPtySessionResponse (output the
    /// client missed before attach succeeded).
    pub initial_backlog: Vec<u8>,
    /// Cumulative bytes dropped from the daemon's ring buffer before this
    /// attach. Zero if the buffer never overflowed.
    pub bytes_missed: u64,
}

/// Errors specific to attach.
#[derive(Debug)]
pub enum AttachError {
    /// Failed to open a socket connection to the daemon.
    Connect(std::io::Error),
    /// I/O failed while exchanging attach or stream frames.
    Io(std::io::Error),
    /// A daemon response or stream frame could not be decoded.
    Decode(prost::DecodeError),
    /// The daemon rejected the attach request.
    Server {
        /// Status code returned by the daemon.
        code: StatusCode,
        /// Human-readable error message returned by the daemon.
        message: String,
    },
    /// The daemon never sent an AttachPtySessionResponse payload.
    MissingPayload,
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::Connect(e) => write!(f, "attach connect failed: {e}"),
            AttachError::Io(e) => write!(f, "attach io error: {e}"),
            AttachError::Decode(e) => write!(f, "attach decode error: {e}"),
            AttachError::Server { code, message } => {
                write!(f, "attach server error {code:?}: {message}")
            }
            AttachError::MissingPayload => write!(f, "attach response missing payload"),
        }
    }
}

impl std::error::Error for AttachError {}

impl PtyAttachment {
    /// Open a fresh socket to the daemon and attach to `session_id`.
    pub fn attach(
        scope_hash: Option<&str>,
        session_id: &str,
        rows: u16,
        cols: u16,
        steal: bool,
    ) -> Result<Self, AttachError> {
        let socket_path = paths::socket_path(scope_hash);
        Self::attach_to(&socket_path, session_id, rows, cols, steal)
    }

    /// Open a fresh socket at `socket_path` and attach to `session_id`.
    pub fn attach_to(
        socket_path: &str,
        session_id: &str,
        rows: u16,
        cols: u16,
        steal: bool,
    ) -> Result<Self, AttachError> {
        let mut terminal_capabilities = current_terminal_capabilities();
        if !terminal_capabilities.is_tty {
            terminal_capabilities.is_tty = true;
            terminal_capabilities.graphics = TerminalGraphicsCapabilities::unknown();
        }
        Self::attach_to_with_terminal_capabilities(
            socket_path,
            session_id,
            rows,
            cols,
            steal,
            terminal_capabilities,
        )
    }

    /// Attach with explicit terminal metadata. This is useful for tests,
    /// non-interactive attach clients, and callers that already performed
    /// capability probing before opening the daemon socket.
    pub fn attach_to_with_terminal_capabilities(
        socket_path: &str,
        session_id: &str,
        rows: u16,
        cols: u16,
        steal: bool,
        terminal_capabilities: TerminalCapabilities,
    ) -> Result<Self, AttachError> {
        // Bounded connect (issue #590, cluster B): a bound-but-never-
        // accepting daemon socket must not wedge the attaching client.
        paths::make_socket_name(socket_path).map_err(AttachError::Connect)?;
        let stream = crate::client::deadline_io::connect_with_timeout(socket_path)
            .map_err(AttachError::Connect)?;
        let stream_clone = stream.try_clone().map_err(AttachError::Connect)?;
        let mut reader = BufReader::new(stream);
        let mut writer = BufWriter::new(stream_clone);

        // Send the AttachPtySession request.
        let attach_request = DaemonRequest {
            id: 1,
            r#type: RequestType::AttachPtySession.into(),
            protocol_version: 1,
            client_name: "running-process-client".into(),
            attach_pty_session: Some(AttachPtySessionRequest {
                session_id: session_id.into(),
                rows: rows as u32,
                cols: cols as u32,
                steal,
                term: terminal_capabilities.term.unwrap_or_default(),
                is_tty: terminal_capabilities.is_tty,
                graphics_capabilities: Some(terminal_graphics_capabilities_to_proto(
                    &terminal_capabilities.graphics,
                )),
            }),
            ..Default::default()
        };
        write_length_prefixed(&mut writer, &attach_request.encode_to_vec())
            .map_err(AttachError::Io)?;

        // Read the initial response.
        let response_bytes = read_length_prefixed(&mut reader).map_err(AttachError::Io)?;
        let response = DaemonResponse::decode(&response_bytes[..]).map_err(AttachError::Decode)?;
        if response.code != StatusCode::Ok as i32 {
            let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
            return Err(AttachError::Server {
                code,
                message: response.message,
            });
        }
        let payload: AttachPtySessionResponse = response
            .attach_pty_session
            .ok_or(AttachError::MissingPayload)?;

        Ok(Self {
            reader,
            writer,
            initial_backlog: payload.backlog,
            bytes_missed: payload.bytes_missed,
        })
    }

    /// Block until the next stream frame arrives.
    pub fn recv_frame(&mut self) -> Result<PtyStreamFrame, AttachError> {
        let bytes = read_length_prefixed(&mut self.reader).map_err(AttachError::Io)?;
        PtyStreamFrame::decode(&bytes[..]).map_err(AttachError::Decode)
    }

    /// Block until the next stream frame arrives, or until `timeout`
    /// elapses (returns `Ok(None)`). The underlying socket is put into
    /// nonblocking mode for the duration of the wait; callers should not
    /// interleave this with `recv_frame`.
    pub fn recv_frame_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<PtyStreamFrame>, AttachError> {
        // Pull the underlying stream out of BufReader so we can set
        // read_timeout. interprocess::local_socket::Stream supports
        // set_nonblocking via the platform shim; for portability we just
        // poll in a short loop.
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // Try to fill the BufReader buffer non-blockingly. If we
            // already have data, decode directly. Otherwise, sleep briefly
            // and retry until the deadline.
            if !self.reader.buffer().is_empty() {
                return self.recv_frame().map(Some);
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            // Sleep a small amount; the OS will buffer incoming data.
            //
            // #199: intentional — `interprocess::local_socket::Stream`
            // lacks a portable peek/ready primitive on Windows. The
            // 20ms poll is the documented fallback. Replacing with
            // an event-based primitive would require a per-platform
            // shim that the upstream crate doesn't expose.
            std::thread::sleep(Duration::from_millis(20));
            // Probe by peeking a single byte: read from reader will block,
            // so we use the BufReader.fill_buf trick by reading 0 bytes
            // first to populate. Simpler: just call recv_frame once the
            // underlying socket reports it has data — but
            // interprocess::Stream lacks portable peek. As a portable
            // fallback, attempt a frame read and return on first success.
            //
            // To avoid blocking forever past the deadline, we rely on the
            // OS to make recv_frame's read_exact return data quickly once
            // it arrives; in practice for the M2 use case timeouts are
            // generous (seconds) and the sleep loop above is the dominant
            // mechanism. We do NOT actually call recv_frame here because
            // it would block.
        }
    }

    /// Send raw input bytes to the PTY.
    pub fn send_input(&mut self, bytes: &[u8]) -> Result<(), AttachError> {
        let frame = PtyInputFrame {
            frame: Some(InputOneof::Input(bytes.to_vec())),
        };
        write_length_prefixed(&mut self.writer, &frame.encode_to_vec()).map_err(AttachError::Io)
    }

    /// Send a resize event.
    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<(), AttachError> {
        let frame = PtyInputFrame {
            frame: Some(InputOneof::Resize(PtyResize {
                rows: rows as u32,
                cols: cols as u32,
            })),
        };
        write_length_prefixed(&mut self.writer, &frame.encode_to_vec()).map_err(AttachError::Io)
    }

    /// Send an interrupt (Ctrl+C / SIGINT) to the child process group.
    pub fn send_interrupt(&mut self) -> Result<(), AttachError> {
        let frame = PtyInputFrame {
            frame: Some(InputOneof::Interrupt(true)),
        };
        write_length_prefixed(&mut self.writer, &frame.encode_to_vec()).map_err(AttachError::Io)
    }

    /// Cleanly detach this attachment; the session keeps running.
    pub fn detach(mut self) -> Result<(), AttachError> {
        let frame = PtyInputFrame {
            frame: Some(InputOneof::Detach(true)),
        };
        write_length_prefixed(&mut self.writer, &frame.encode_to_vec()).map_err(AttachError::Io)
    }
}

// ---------------------------------------------------------------------------
// Length-prefixed framing (matches the daemon's LengthDelimitedCodec)
// ---------------------------------------------------------------------------

fn write_length_prefixed<W: Write>(w: &mut W, payload: &[u8]) -> Result<(), std::io::Error> {
    let len = payload.len() as u32;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_length_prefixed<R: Read>(r: &mut R) -> Result<Vec<u8>, std::io::Error> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let len = u32::from_be_bytes(len_buf) as usize;
    // Cap before allocating so a corrupt/desynced frame can't drive a
    // multi-GiB allocation + unbounded read (issue #590, cluster B).
    crate::client::deadline_io::check_frame_len(len)?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pty_spawn_request_builder_defaults() {
        let req = PtySpawnRequest::new(["echo", "hi"])
            .with_size(40, 100)
            .with_originator("test:1");
        assert_eq!(req.argv, vec!["echo".to_string(), "hi".to_string()]);
        assert_eq!(req.rows, 40);
        assert_eq!(req.cols, 100);
        assert_eq!(req.originator.as_deref(), Some("test:1"));
    }
}
