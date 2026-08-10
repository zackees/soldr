use std::ffi::OsString;
use std::io;
use std::path::PathBuf;

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt;

use crate::daemon::pipe_sessions::PipeStreamSelect;
use crate::daemon::telemetry::{
    TeeBackpressure, TeeFileMode, TeeFileOptions, TeeHandle, TeeStatus, TeeStream,
};
use crate::proto::daemon::{
    DaemonRequest, DaemonResponse, GetSessionTeeStatusResponse, RegisterSessionTeeResponse,
    StatusCode, TeeBackpressure as ProtoTeeBackpressure, TeeFileMode as ProtoTeeFileMode,
    TeeSessionKind, TeeSinkKind, TeeStreamKind, UnregisterSessionTeeResponse,
};

use super::util::error_pty_response;
use super::DaemonState;

pub fn handle_register_session_tee(request: &DaemonRequest, state: &DaemonState) -> DaemonResponse {
    let req = match request.register_session_tee.as_ref() {
        Some(req) => req,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "register_session_tee payload missing".into(),
            );
        }
    };

    if req.session_id.is_empty() {
        return error_pty_response(
            request.id,
            StatusCode::InvalidArgument,
            "session_id must not be empty".into(),
        );
    }

    let sink_kind = match TeeSinkKind::try_from(req.sink_kind) {
        Ok(kind) => kind,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid tee sink kind".into(),
            );
        }
    };
    if sink_kind != TeeSinkKind::File {
        return error_pty_response(
            request.id,
            StatusCode::InvalidArgument,
            "only file tee sinks are supported over IPC".into(),
        );
    }

    let stream = match TeeStreamKind::try_from(req.stream) {
        Ok(stream) => stream,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid tee stream".into(),
            );
        }
    };
    let path = match decode_os_path(&req.file_path) {
        Ok(path) => path,
        Err(e) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                format!("invalid tee file path: {e}"),
            );
        }
    };
    if path.as_os_str().is_empty() {
        return error_pty_response(
            request.id,
            StatusCode::InvalidArgument,
            "tee file path must not be empty".into(),
        );
    }

    let options = match file_options(
        req.file_mode,
        req.queue_capacity,
        req.suppress_missed_markers,
        req.backpressure,
    ) {
        Ok(options) => options,
        Err(message) => {
            return error_pty_response(request.id, StatusCode::InvalidArgument, message);
        }
    };

    let session_kind = match TeeSessionKind::try_from(req.session_kind) {
        Ok(kind) => kind,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid tee session kind".into(),
            );
        }
    };

    let result = match session_kind {
        TeeSessionKind::Pty => {
            register_pty_file_tee(state, &req.session_id, stream, &path, options)
        }
        TeeSessionKind::Pipe => {
            register_pipe_file_tee(state, &req.session_id, stream, &path, options)
        }
        TeeSessionKind::Unspecified => Err(RegistrationError::Invalid(
            "session_kind must be PTY or PIPE".into(),
        )),
    };

    match result {
        Ok(handle) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: "OK".into(),
            register_session_tee: Some(RegisterSessionTeeResponse {
                tee_handle: handle.as_u64(),
            }),
            ..Default::default()
        },
        Err(err) => err.into_response(request.id),
    }
}

pub fn handle_unregister_session_tee(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let req = match request.unregister_session_tee.as_ref() {
        Some(req) => req,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "unregister_session_tee payload missing".into(),
            );
        }
    };

    let session_kind = match TeeSessionKind::try_from(req.session_kind) {
        Ok(kind) => kind,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid tee session kind".into(),
            );
        }
    };
    let handle = TeeHandle::from_u64(req.tee_handle);
    let result = match session_kind {
        TeeSessionKind::Pty => state
            .pty_sessions
            .get(&req.session_id)
            .ok_or_else(|| RegistrationError::NotFound("PTY session not found".into()))
            .and_then(|session| {
                session
                    .untee(handle)
                    .then_some(())
                    .ok_or_else(|| RegistrationError::NotFound("tee handle not found".into()))
            }),
        TeeSessionKind::Pipe => state
            .pipe_sessions
            .get(&req.session_id)
            .ok_or_else(|| RegistrationError::NotFound("pipe session not found".into()))
            .and_then(|session| {
                session
                    .untee(handle)
                    .then_some(())
                    .ok_or_else(|| RegistrationError::NotFound("tee handle not found".into()))
            }),
        TeeSessionKind::Unspecified => Err(RegistrationError::Invalid(
            "session_kind must be PTY or PIPE".into(),
        )),
    };

    match result {
        Ok(()) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: "OK".into(),
            unregister_session_tee: Some(UnregisterSessionTeeResponse::default()),
            ..Default::default()
        },
        Err(err) => err.into_response(request.id),
    }
}

pub fn handle_get_session_tee_status(
    request: &DaemonRequest,
    state: &DaemonState,
) -> DaemonResponse {
    let req = match request.get_session_tee_status.as_ref() {
        Some(req) => req,
        None => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "get_session_tee_status payload missing".into(),
            );
        }
    };

    let session_kind = match TeeSessionKind::try_from(req.session_kind) {
        Ok(kind) => kind,
        Err(_) => {
            return error_pty_response(
                request.id,
                StatusCode::InvalidArgument,
                "invalid tee session kind".into(),
            );
        }
    };
    let handle = TeeHandle::from_u64(req.tee_handle);
    let result = match session_kind {
        TeeSessionKind::Pty => state
            .pty_sessions
            .get(&req.session_id)
            .ok_or_else(|| RegistrationError::NotFound("PTY session not found".into()))
            .and_then(|session| {
                session
                    .tee_status(handle)
                    .ok_or_else(|| RegistrationError::NotFound("tee handle not found".into()))
            }),
        TeeSessionKind::Pipe => state
            .pipe_sessions
            .get(&req.session_id)
            .ok_or_else(|| RegistrationError::NotFound("pipe session not found".into()))
            .and_then(|session| {
                session
                    .tee_status(handle)
                    .ok_or_else(|| RegistrationError::NotFound("tee handle not found".into()))
            }),
        TeeSessionKind::Unspecified => Err(RegistrationError::Invalid(
            "session_kind must be PTY or PIPE".into(),
        )),
    };

    match result {
        Ok(status) => DaemonResponse {
            request_id: request.id,
            code: StatusCode::Ok as i32,
            message: "OK".into(),
            get_session_tee_status: Some(status_response(status)),
            ..Default::default()
        },
        Err(err) => err.into_response(request.id),
    }
}

fn register_pty_file_tee(
    state: &DaemonState,
    session_id: &str,
    stream: TeeStreamKind,
    path: &PathBuf,
    options: TeeFileOptions,
) -> Result<TeeHandle, RegistrationError> {
    let session = state
        .pty_sessions
        .get(session_id)
        .ok_or_else(|| RegistrationError::NotFound("PTY session not found".into()))?;
    match stream {
        TeeStreamKind::PtyOutput => session
            .tee_output_file(path, options)
            .map_err(RegistrationError::from_io),
        TeeStreamKind::Stdin => session
            .tee_input_file(path, options)
            .map_err(RegistrationError::from_io),
        TeeStreamKind::Stdout | TeeStreamKind::Stderr | TeeStreamKind::Unspecified => Err(
            RegistrationError::Invalid("PTY tee stream must be PTY_OUTPUT or STDIN".into()),
        ),
    }
}

fn register_pipe_file_tee(
    state: &DaemonState,
    session_id: &str,
    stream: TeeStreamKind,
    path: &PathBuf,
    options: TeeFileOptions,
) -> Result<TeeHandle, RegistrationError> {
    let session = state
        .pipe_sessions
        .get(session_id)
        .ok_or_else(|| RegistrationError::NotFound("pipe session not found".into()))?;
    match stream {
        TeeStreamKind::Stdout => session
            .tee_stream_file(PipeStreamSelect::Stdout, path, options)
            .map_err(RegistrationError::from_io),
        TeeStreamKind::Stderr => session
            .tee_stream_file(PipeStreamSelect::Stderr, path, options)
            .map_err(RegistrationError::from_io),
        TeeStreamKind::Stdin => session
            .tee_input_file(path, options)
            .map_err(RegistrationError::from_io),
        TeeStreamKind::PtyOutput | TeeStreamKind::Unspecified => Err(RegistrationError::Invalid(
            "pipe tee stream must be STDOUT, STDERR, or STDIN".into(),
        )),
    }
}

fn file_options(
    file_mode: i32,
    queue_capacity: u32,
    suppress_missed_markers: bool,
    backpressure: i32,
) -> Result<TeeFileOptions, String> {
    let mode = match ProtoTeeFileMode::try_from(file_mode).map_err(|_| "invalid tee file mode")? {
        ProtoTeeFileMode::Append => TeeFileMode::Append,
        ProtoTeeFileMode::Truncate => TeeFileMode::Truncate,
    };
    let backpressure =
        match ProtoTeeBackpressure::try_from(backpressure).map_err(|_| "invalid backpressure")? {
            ProtoTeeBackpressure::DropOldest => TeeBackpressure::DropOldest,
            ProtoTeeBackpressure::Block => TeeBackpressure::Block,
        };
    let mut options = TeeFileOptions {
        mode,
        backpressure,
        write_missed_markers: !suppress_missed_markers,
        ..TeeFileOptions::default()
    };
    if queue_capacity != 0 {
        options.queue_capacity = queue_capacity as usize;
    }
    Ok(options)
}

fn status_response(status: TeeStatus) -> GetSessionTeeStatusResponse {
    GetSessionTeeStatusResponse {
        stream: match status.stream {
            TeeStream::PtyOutput => TeeStreamKind::PtyOutput as i32,
            TeeStream::Stdout => TeeStreamKind::Stdout as i32,
            TeeStream::Stderr => TeeStreamKind::Stderr as i32,
            TeeStream::Stdin => TeeStreamKind::Stdin as i32,
        },
        missed_bytes: status.missed_bytes,
        disconnected: status.disconnected,
    }
}

#[cfg(unix)]
fn decode_os_path(bytes: &[u8]) -> io::Result<PathBuf> {
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

#[cfg(windows)]
fn decode_os_path(bytes: &[u8]) -> io::Result<PathBuf> {
    if bytes.len() % 2 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows path bytes must be little-endian UTF-16",
        ));
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    Ok(PathBuf::from(OsString::from_wide(&wide)))
}

enum RegistrationError {
    Invalid(String),
    NotFound(String),
    Io(io::Error),
}

impl RegistrationError {
    fn from_io(error: io::Error) -> Self {
        if error.kind() == io::ErrorKind::InvalidInput {
            Self::Invalid(error.to_string())
        } else {
            Self::Io(error)
        }
    }

    fn into_response(self, request_id: u64) -> DaemonResponse {
        match self {
            Self::Invalid(message) => {
                error_pty_response(request_id, StatusCode::InvalidArgument, message)
            }
            Self::NotFound(message) => {
                error_pty_response(request_id, StatusCode::NotFound, message)
            }
            Self::Io(error) => {
                error_pty_response(request_id, StatusCode::Internal, error.to_string())
            }
        }
    }
}
