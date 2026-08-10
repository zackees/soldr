//! Client helpers for daemon-owned session tee telemetry.

use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

use crate::client::{ClientError, DaemonClient};
use crate::proto::daemon::{
    DaemonRequest, GetSessionTeeStatusRequest, RegisterSessionTeeRequest, RequestType, StatusCode,
    TeeBackpressure as ProtoTeeBackpressure, TeeFileMode as ProtoTeeFileMode,
    TeeSessionKind as ProtoTeeSessionKind, TeeSinkKind, TeeStreamKind as ProtoTeeStreamKind,
    UnregisterSessionTeeRequest,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Session transport that owns the stream being tee'd.
pub enum SessionTeeKind {
    /// Daemon-owned pseudo-terminal session.
    Pty,
    /// Daemon-owned pipe-backed session.
    Pipe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Session byte stream that can be mirrored to a tee sink.
pub enum SessionTeeStream {
    /// Combined PTY output bytes.
    PtyOutput,
    /// Pipe session standard output bytes.
    Stdout,
    /// Pipe session standard error bytes.
    Stderr,
    /// Bytes written successfully to a pipe session's standard input.
    Stdin,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// File open mode used when registering a file tee.
pub enum SessionTeeFileMode {
    /// Append tee output to the file if it already exists.
    Append,
    /// Truncate the file before writing tee output.
    Truncate,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Backpressure policy for bounded tee queues.
pub enum SessionTeeBackpressure {
    /// Keep the session reader non-blocking and account for dropped bytes.
    DropOldest,
    /// Block the session reader until the tee sink accepts more bytes.
    Block,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Request used to register a daemon-managed file tee for a session stream.
pub struct SessionTeeFileRequest {
    /// Session identifier returned by the spawn API.
    pub session_id: String,
    /// Kind of session that owns `session_id`.
    pub session_kind: SessionTeeKind,
    /// Stream to mirror into the file.
    pub stream: SessionTeeStream,
    /// Destination file path on the daemon host.
    pub path: PathBuf,
    /// File open mode for the destination path.
    pub mode: SessionTeeFileMode,
    /// 0 means use the daemon default.
    pub queue_capacity: u32,
    /// Whether the daemon writes marker lines when tee bytes are missed.
    pub write_missed_markers: bool,
    /// Queue behavior when the file sink cannot keep up.
    pub backpressure: SessionTeeBackpressure,
}

impl SessionTeeFileRequest {
    /// Create a file tee request with append mode and daemon-default queue size.
    pub fn new<P>(
        session_id: impl Into<String>,
        session_kind: SessionTeeKind,
        stream: SessionTeeStream,
        path: P,
    ) -> Self
    where
        P: AsRef<Path>,
    {
        Self {
            session_id: session_id.into(),
            session_kind,
            stream,
            path: path.as_ref().to_path_buf(),
            mode: SessionTeeFileMode::Append,
            queue_capacity: 0,
            write_missed_markers: true,
            backpressure: SessionTeeBackpressure::DropOldest,
        }
    }

    /// Open the destination file in truncate mode instead of append mode.
    pub fn truncate(mut self) -> Self {
        self.mode = SessionTeeFileMode::Truncate;
        self
    }

    /// Set the bounded queue capacity; `0` keeps the daemon default.
    pub fn queue_capacity(mut self, capacity: u32) -> Self {
        self.queue_capacity = capacity;
        self
    }

    /// Disable marker lines for bytes missed by the file tee.
    pub fn suppress_missed_markers(mut self) -> Self {
        self.write_missed_markers = false;
        self
    }

    /// Set the queue backpressure policy for this tee.
    pub fn backpressure(mut self, backpressure: SessionTeeBackpressure) -> Self {
        self.backpressure = backpressure;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Current daemon status for a registered session tee.
pub struct SessionTeeStatus {
    /// Stream associated with the registered tee handle.
    pub stream: SessionTeeStream,
    /// Number of stream bytes missed by this tee.
    pub missed_bytes: u64,
    /// Whether the tee sink has disconnected from the session stream.
    pub disconnected: bool,
}

impl DaemonClient {
    /// Register a daemon-owned file tee and return its opaque tee handle.
    pub fn register_session_file_tee(
        &mut self,
        request: &SessionTeeFileRequest,
    ) -> Result<u64, ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::RegisterSessionTee.into(),
            protocol_version: 1,
            register_session_tee: Some(RegisterSessionTeeRequest {
                session_id: request.session_id.clone(),
                session_kind: proto_session_kind(request.session_kind) as i32,
                stream: proto_stream_kind(request.stream) as i32,
                sink_kind: TeeSinkKind::File as i32,
                file_path: encode_os_path(&request.path),
                file_mode: proto_file_mode(request.mode) as i32,
                queue_capacity: request.queue_capacity,
                suppress_missed_markers: !request.write_missed_markers,
                backpressure: proto_backpressure(request.backpressure) as i32,
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)?;
        let payload = response
            .register_session_tee
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "register_session_tee response missing payload".into(),
            })?;
        Ok(payload.tee_handle)
    }

    /// Unregister a previously registered session tee handle.
    pub fn unregister_session_tee(
        &mut self,
        session_kind: SessionTeeKind,
        session_id: &str,
        tee_handle: u64,
    ) -> Result<(), ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::UnregisterSessionTee.into(),
            protocol_version: 1,
            unregister_session_tee: Some(UnregisterSessionTeeRequest {
                session_id: session_id.to_string(),
                session_kind: proto_session_kind(session_kind) as i32,
                tee_handle,
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)
    }

    /// Fetch the current status for a registered session tee handle.
    pub fn get_session_tee_status(
        &mut self,
        session_kind: SessionTeeKind,
        session_id: &str,
        tee_handle: u64,
    ) -> Result<SessionTeeStatus, ClientError> {
        let daemon_request = DaemonRequest {
            id: self.next_request_id(),
            r#type: RequestType::GetSessionTeeStatus.into(),
            protocol_version: 1,
            get_session_tee_status: Some(GetSessionTeeStatusRequest {
                session_id: session_id.to_string(),
                session_kind: proto_session_kind(session_kind) as i32,
                tee_handle,
            }),
            ..Default::default()
        };
        let response = self.send_request(daemon_request)?;
        ensure_ok(&response)?;
        let payload = response
            .get_session_tee_status
            .ok_or_else(|| ClientError::Server {
                code: StatusCode::Internal,
                message: "get_session_tee_status response missing payload".into(),
            })?;
        let stream =
            ProtoTeeStreamKind::try_from(payload.stream).map_err(|_| ClientError::Server {
                code: StatusCode::Internal,
                message: "get_session_tee_status response has invalid stream".into(),
            })?;
        Ok(SessionTeeStatus {
            stream: client_stream_kind(stream)?,
            missed_bytes: payload.missed_bytes,
            disconnected: payload.disconnected,
        })
    }
}

fn ensure_ok(response: &crate::proto::daemon::DaemonResponse) -> Result<(), ClientError> {
    if response.code == StatusCode::Ok as i32 {
        return Ok(());
    }
    let code = StatusCode::try_from(response.code).unwrap_or(StatusCode::UnknownRequest);
    Err(ClientError::Server {
        code,
        message: response.message.clone(),
    })
}

fn proto_session_kind(kind: SessionTeeKind) -> ProtoTeeSessionKind {
    match kind {
        SessionTeeKind::Pty => ProtoTeeSessionKind::Pty,
        SessionTeeKind::Pipe => ProtoTeeSessionKind::Pipe,
    }
}

fn proto_stream_kind(stream: SessionTeeStream) -> ProtoTeeStreamKind {
    match stream {
        SessionTeeStream::PtyOutput => ProtoTeeStreamKind::PtyOutput,
        SessionTeeStream::Stdout => ProtoTeeStreamKind::Stdout,
        SessionTeeStream::Stderr => ProtoTeeStreamKind::Stderr,
        SessionTeeStream::Stdin => ProtoTeeStreamKind::Stdin,
    }
}

fn client_stream_kind(stream: ProtoTeeStreamKind) -> Result<SessionTeeStream, ClientError> {
    match stream {
        ProtoTeeStreamKind::PtyOutput => Ok(SessionTeeStream::PtyOutput),
        ProtoTeeStreamKind::Stdout => Ok(SessionTeeStream::Stdout),
        ProtoTeeStreamKind::Stderr => Ok(SessionTeeStream::Stderr),
        ProtoTeeStreamKind::Stdin => Ok(SessionTeeStream::Stdin),
        ProtoTeeStreamKind::Unspecified => Err(ClientError::Server {
            code: StatusCode::Internal,
            message: "get_session_tee_status response has unspecified stream".into(),
        }),
    }
}

fn proto_file_mode(mode: SessionTeeFileMode) -> ProtoTeeFileMode {
    match mode {
        SessionTeeFileMode::Append => ProtoTeeFileMode::Append,
        SessionTeeFileMode::Truncate => ProtoTeeFileMode::Truncate,
    }
}

fn proto_backpressure(backpressure: SessionTeeBackpressure) -> ProtoTeeBackpressure {
    match backpressure {
        SessionTeeBackpressure::DropOldest => ProtoTeeBackpressure::DropOldest,
        SessionTeeBackpressure::Block => ProtoTeeBackpressure::Block,
    }
}

#[cfg(unix)]
fn encode_os_path(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(windows)]
fn encode_os_path(path: &Path) -> Vec<u8> {
    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect()
}
