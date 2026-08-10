//! Endpoint and process identity checks for backend handles.

use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use prost::Message;

use crate::broker::backend_lifecycle::identity::{DaemonProcess, IdentityError};
use crate::broker::backend_lifecycle::verify_pid::{self, ProcessHandle, VerifyPidError};
use crate::broker::protocol::{
    self, read_frame, write_frame, Endpoint, Frame, FrameKind, FramingError, PayloadEncoding,
    ENVELOPE_VERSION, MAX_FRAME_BYTES, PROTOCOL_VERSION,
};

/// Byte length of the random challenge carried by endpoint probe requests.
pub const PROBE_NONCE_BYTES: usize = 32;
const NONBLOCKING_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Payload protocol reserved for `BackendHandle` endpoint identity probes.
///
/// Re-exported from the authoritative
/// [`registry`](crate::broker::protocol::registry), which owns every v1
/// payload-protocol ID (#375).
pub use crate::broker::protocol::registry::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;

/// Default deadline for the active endpoint-response proof.
pub const DEFAULT_ENDPOINT_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Verify that an endpoint refers to the expected daemon process.
pub fn probe_endpoint(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
) -> Result<ProcessHandle, ProbeError> {
    if !same_endpoint(endpoint, &expected.ipc_endpoint) {
        return Err(ProbeError::EndpointMismatch);
    }
    let process_handle =
        verify_pid::verify_daemon_process(expected).map_err(ProbeError::VerifyPid)?;
    probe_endpoint_response(endpoint, expected)?;
    Ok(process_handle)
}

/// Compare two endpoint identities exactly.
pub fn same_endpoint(left: &Endpoint, right: &Endpoint) -> bool {
    left.namespace_id == right.namespace_id && left.path == right.path
}

/// Actively probe a backend endpoint and verify that it returns the expected
/// daemon identity.
///
/// The probe uses the broker v1 frame layout with a dedicated payload protocol.
/// Requests carry a 32-byte nonce. Responses must echo that nonce and include a
/// prost-encoded `DaemonProcess` payload that exactly matches `expected`.
pub fn probe_endpoint_response(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
) -> Result<(), EndpointProbeError> {
    probe_endpoint_response_with_timeout(endpoint, expected, DEFAULT_ENDPOINT_PROBE_TIMEOUT)
}

/// Timed variant of [`probe_endpoint_response`] used by tests and diagnostics.
pub fn probe_endpoint_response_with_timeout(
    endpoint: &Endpoint,
    expected: &DaemonProcess,
    timeout: Duration,
) -> Result<(), EndpointProbeError> {
    let mut nonce = [0_u8; PROBE_NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(EndpointProbeError::Random)?;
    let request_id = u64::from_le_bytes(nonce[..8].try_into().expect("nonce has 8 bytes"));
    let request_frame = endpoint_probe_request_frame(request_id, &nonce);
    let mut request_bytes = Vec::new();
    request_frame
        .encode(&mut request_bytes)
        .map_err(EndpointProbeError::EncodeFrame)?;

    let deadline = Instant::now() + timeout;
    let mut stream = connect_endpoint_with_deadline(endpoint, deadline)?;
    stream
        .set_nonblocking(true)
        .map_err(EndpointProbeError::ConfigureNonblocking)?;
    write_probe_frame_with_deadline(&mut stream, &request_bytes, deadline)?;

    let response_bytes = read_probe_frame_with_deadline(&mut stream, deadline)?;
    let response_frame =
        Frame::decode(response_bytes.as_slice()).map_err(EndpointProbeError::DecodeFrame)?;
    validate_endpoint_probe_response_frame(&response_frame, request_id)?;
    let actual = decode_response_identity(&response_frame.payload, &nonce)?;
    if !same_daemon_identity(&actual, expected) {
        return Err(identity_mismatch(expected, &actual));
    }
    Ok(())
}

/// Decoded endpoint probe request for backend-side responders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointProbeRequest {
    /// Request frame ID that the response must echo.
    pub request_id: u64,
    /// Random challenge that the response must echo.
    pub nonce: [u8; PROBE_NONCE_BYTES],
    /// Trace context copied from the request frame, if any.
    pub traceparent: String,
    /// Trace state copied from the request frame, if any.
    pub tracestate: String,
}

/// Read and validate one endpoint probe request from an accepted IPC stream.
pub fn read_endpoint_probe_request<S: Read>(
    stream: &mut S,
) -> Result<EndpointProbeRequest, EndpointProbeServerError> {
    let request_bytes = read_frame(stream)?;
    let frame =
        Frame::decode(request_bytes.as_slice()).map_err(EndpointProbeServerError::DecodeFrame)?;
    endpoint_probe_request_from_frame(&frame)
}

/// Validate an already-decoded frame as an endpoint probe request.
///
/// Exposed for the [`crate::broker::backend_sdk`] endpoint mux (#412),
/// which decodes frames from a consumer-owned read buffer instead of an
/// exclusive stream.
pub fn endpoint_probe_request_from_frame(
    frame: &Frame,
) -> Result<EndpointProbeRequest, EndpointProbeServerError> {
    validate_endpoint_probe_request_frame(frame)?;
    let nonce = frame
        .payload
        .as_slice()
        .try_into()
        .map_err(|_| EndpointProbeServerError::MalformedPayload("nonce must be 32 bytes"))?;
    Ok(EndpointProbeRequest {
        request_id: frame.request_id,
        nonce,
        traceparent: frame.traceparent.clone(),
        tracestate: frame.tracestate.clone(),
    })
}

/// Write one endpoint probe response for a validated request.
pub fn write_endpoint_probe_response<S: Write>(
    stream: &mut S,
    request: &EndpointProbeRequest,
    daemon: &DaemonProcess,
) -> Result<(), EndpointProbeServerError> {
    let response_frame = endpoint_probe_response_frame(request, daemon);
    let mut response_bytes = Vec::new();
    response_frame
        .encode(&mut response_bytes)
        .map_err(EndpointProbeServerError::EncodeFrame)?;
    write_frame(stream, &response_bytes)?;
    Ok(())
}

/// Serve exactly one endpoint probe request on an already-accepted IPC stream.
pub fn handle_endpoint_probe<S: Read + Write>(
    stream: &mut S,
    daemon: &DaemonProcess,
) -> Result<(), EndpointProbeServerError> {
    let request = read_endpoint_probe_request(stream)?;
    write_endpoint_probe_response(stream, &request, daemon)
}

/// Errors returned while probing a backend endpoint.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The caller-provided endpoint did not match the expected daemon endpoint.
    #[error("endpoint does not match expected daemon identity")]
    EndpointMismatch,
    /// The endpoint did not answer the active identity probe as expected.
    #[error(transparent)]
    EndpointResponse(#[from] EndpointProbeError),
    /// The daemon process identity could not be verified.
    #[error(transparent)]
    VerifyPid(#[from] VerifyPidError),
}

/// Errors returned by the active endpoint-response probe.
#[derive(Debug, thiserror::Error)]
pub enum EndpointProbeError {
    /// The probe nonce could not be generated.
    #[error("backend endpoint probe random generation failed: {0}")]
    Random(getrandom::Error),
    /// The endpoint path/name could not be converted to a local socket name.
    #[error("backend endpoint probe local-socket name failed: {0}")]
    LocalSocketName(io::Error),
    /// Connecting to the endpoint failed.
    #[error("backend endpoint probe connect failed: {0}")]
    Connect(io::Error),
    /// The stream could not be switched to nonblocking mode for deadline I/O.
    #[error("backend endpoint probe nonblocking setup failed: {0}")]
    ConfigureNonblocking(io::Error),
    /// Probe I/O exceeded the configured deadline.
    #[error("backend endpoint probe timed out")]
    Timeout,
    /// Raw probe I/O failed.
    #[error("backend endpoint probe I/O failed: {0}")]
    Io(io::Error),
    /// The peer used the wrong broker framing byte.
    #[error("backend endpoint probe unsupported framing version: got {got}, expected {expected}")]
    UnsupportedFramingVersion {
        /// Framing byte received from the peer.
        got: u8,
        /// Framing byte expected by v1.
        expected: u8,
    },
    /// The peer advertised a frame that exceeds the v1 frame cap.
    #[error("backend endpoint probe frame body too large: {body_length} bytes exceeds cap {cap}")]
    FrameTooLarge {
        /// Advertised frame body length.
        body_length: usize,
        /// Maximum accepted frame body length.
        cap: usize,
    },
    /// The outbound probe request frame could not be encoded.
    #[error("failed to encode endpoint probe frame: {0}")]
    EncodeFrame(prost::EncodeError),
    /// The response frame could not be decoded.
    #[error("failed to decode endpoint probe response Frame: {0}")]
    DecodeFrame(prost::DecodeError),
    /// The response frame did not match the endpoint-probe contract.
    #[error("unexpected endpoint probe response: {0}")]
    UnexpectedFrame(&'static str),
    /// The response payload did not match the endpoint-probe contract.
    #[error("endpoint probe response payload is malformed: {0}")]
    MalformedPayload(&'static str),
    /// The response daemon identity could not be decoded.
    #[error("failed to decode endpoint probe daemon identity: {0}")]
    DecodeDaemonProcess(prost::DecodeError),
    /// The response daemon identity was malformed.
    #[error(transparent)]
    Identity(#[from] IdentityError),
    /// The response daemon identity did not match the expected identity.
    #[error("endpoint probe response identity did not match expected daemon identity: {field}")]
    IdentityMismatch {
        /// First mismatched identity field.
        field: &'static str,
    },
}

/// Errors returned by backend-side endpoint probe responders.
#[derive(Debug, thiserror::Error)]
pub enum EndpointProbeServerError {
    /// v1 framing failed.
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// The request frame could not be decoded.
    #[error("failed to decode endpoint probe request Frame: {0}")]
    DecodeFrame(prost::DecodeError),
    /// The response frame could not be encoded.
    #[error("failed to encode endpoint probe response Frame: {0}")]
    EncodeFrame(prost::EncodeError),
    /// The request frame did not match the endpoint-probe contract.
    #[error("unexpected endpoint probe request: {0}")]
    UnexpectedFrame(&'static str),
    /// The request payload did not match the endpoint-probe contract.
    #[error("endpoint probe request payload is malformed: {0}")]
    MalformedPayload(&'static str),
}

fn endpoint_probe_request_frame(request_id: u64, nonce: &[u8; PROBE_NONCE_BYTES]) -> Frame {
    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        payload: nonce.to_vec(),
        request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

/// Build the endpoint-probe response frame for a validated request.
///
/// Exposed for the [`crate::broker::backend_sdk`] endpoint mux (#412),
/// which answers probes from a consumer-owned read buffer instead of an
/// exclusive stream.
pub fn endpoint_probe_response_frame(
    request: &EndpointProbeRequest,
    daemon: &DaemonProcess,
) -> Frame {
    let mut payload = Vec::with_capacity(PROBE_NONCE_BYTES + 128);
    payload.extend_from_slice(&request.nonce);
    daemon.to_proto().encode(&mut payload).expect(
        "prost encoding DaemonProcess into Vec cannot fail because Vec writes are infallible",
    );

    Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Response as i32,
        payload_protocol: BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
        payload,
        request_id: request.request_id,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: request.traceparent.clone(),
        tracestate: request.tracestate.clone(),
    }
}

/// Validate one frame against the endpoint-probe request contract.
///
/// Exposed for the [`crate::broker::backend_sdk`] endpoint mux (#412).
pub fn validate_endpoint_probe_request_frame(
    frame: &Frame,
) -> Result<(), EndpointProbeServerError> {
    if frame.envelope_version != PROTOCOL_VERSION {
        return Err(EndpointProbeServerError::UnexpectedFrame(
            "envelope_version is not v1",
        ));
    }
    if FrameKind::try_from(frame.kind) != Ok(FrameKind::Request) {
        return Err(EndpointProbeServerError::UnexpectedFrame(
            "kind is not REQUEST",
        ));
    }
    if frame.payload_protocol != BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL {
        return Err(EndpointProbeServerError::UnexpectedFrame(
            "payload_protocol is not endpoint probe",
        ));
    }
    if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
        return Err(EndpointProbeServerError::UnexpectedFrame(
            "payload is compressed",
        ));
    }
    if frame.payload.len() != PROBE_NONCE_BYTES {
        return Err(EndpointProbeServerError::MalformedPayload(
            "nonce must be 32 bytes",
        ));
    }
    Ok(())
}

fn validate_endpoint_probe_response_frame(
    frame: &Frame,
    request_id: u64,
) -> Result<(), EndpointProbeError> {
    if frame.envelope_version != PROTOCOL_VERSION {
        return Err(EndpointProbeError::UnexpectedFrame(
            "envelope_version is not v1",
        ));
    }
    if FrameKind::try_from(frame.kind) != Ok(FrameKind::Response) {
        return Err(EndpointProbeError::UnexpectedFrame("kind is not RESPONSE"));
    }
    if frame.payload_protocol != BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL {
        return Err(EndpointProbeError::UnexpectedFrame(
            "payload_protocol is not endpoint probe",
        ));
    }
    if frame.request_id != request_id {
        return Err(EndpointProbeError::UnexpectedFrame(
            "request_id does not match endpoint probe request",
        ));
    }
    if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
        return Err(EndpointProbeError::UnexpectedFrame("payload is compressed"));
    }
    Ok(())
}

/// Decode the identity payload of one endpoint probe response.
///
/// The payload is untrusted (it comes from whatever process answered the
/// probed endpoint — this is the squat-detection path): a 32-byte nonce echo
/// followed by a prost-encoded [`protocol::DaemonProcess`]. The nonce must
/// match `expected_nonce` before the identity bytes are decoded and
/// normalized through [`DaemonProcess::try_from`]. Exposed for fuzzing.
pub fn decode_response_identity(
    payload: &[u8],
    expected_nonce: &[u8; PROBE_NONCE_BYTES],
) -> Result<DaemonProcess, EndpointProbeError> {
    if payload.len() < PROBE_NONCE_BYTES {
        return Err(EndpointProbeError::MalformedPayload(
            "payload shorter than nonce",
        ));
    }
    let (nonce, identity_bytes) = payload.split_at(PROBE_NONCE_BYTES);
    if nonce != expected_nonce {
        return Err(EndpointProbeError::UnexpectedFrame(
            "nonce does not match endpoint probe request",
        ));
    }
    let proto_identity = protocol::DaemonProcess::decode(identity_bytes)
        .map_err(EndpointProbeError::DecodeDaemonProcess)?;
    DaemonProcess::try_from(proto_identity).map_err(EndpointProbeError::Identity)
}

fn identity_mismatch(expected: &DaemonProcess, actual: &DaemonProcess) -> EndpointProbeError {
    let field = if actual.pid != expected.pid {
        "pid"
    } else if actual.exe_path != expected.exe_path {
        "exe_path"
    } else if actual.exe_sha256 != expected.exe_sha256 {
        "exe_sha256"
    } else if actual.boot_id != expected.boot_id {
        "boot_id"
    } else if !same_endpoint(&actual.ipc_endpoint, &expected.ipc_endpoint) {
        "ipc_endpoint"
    } else {
        "unknown"
    };
    EndpointProbeError::IdentityMismatch { field }
}

fn same_daemon_identity(left: &DaemonProcess, right: &DaemonProcess) -> bool {
    left.pid == right.pid
        && left.exe_path == right.exe_path
        && left.exe_sha256 == right.exe_sha256
        && left.boot_id == right.boot_id
        && same_endpoint(&left.ipc_endpoint, &right.ipc_endpoint)
}

/// Connect to the probe endpoint with a hard deadline.
///
/// `interprocess::local_socket::Stream::connect` is a blocking syscall with
/// no portable timeout: on macOS a bound-but-never-accepted Unix socket can
/// park the caller in `connect(2)` indefinitely once the (tiny) listen
/// backlog is full, which would silently wedge the broker serve thread
/// before it ever binds its own control socket (#399). Run the blocking
/// connect on a helper thread and bound the wait with the probe deadline;
/// on timeout the helper thread owns (and eventually drops) the abandoned
/// stream — the same leak-on-timeout pattern as the client handoff wait.
fn connect_endpoint_with_deadline(
    endpoint: &Endpoint,
    deadline: Instant,
) -> Result<interprocess::local_socket::Stream, EndpointProbeError> {
    if endpoint.path.is_empty() {
        return Err(EndpointProbeError::Connect(io::Error::new(
            io::ErrorKind::InvalidInput,
            "backend endpoint path is empty",
        )));
    }
    // Validate the name synchronously so naming errors keep their own variant.
    endpoint_name(&endpoint.path).map_err(EndpointProbeError::LocalSocketName)?;

    let path = endpoint.path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::Builder::new()
        .name("rp-endpoint-probe-connect".to_string())
        .spawn(move || {
            let result = match endpoint_name(&path) {
                Ok(name) => interprocess::local_socket::Stream::connect(name),
                Err(err) => Err(err),
            };
            // Receiver gone means the probe timed out; drop the stream here.
            let _ = tx.send(result);
        })
        .map_err(EndpointProbeError::Connect)?;

    let remaining = deadline.saturating_duration_since(Instant::now());
    match rx.recv_timeout(remaining) {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => Err(EndpointProbeError::Connect(err)),
        Err(_) => Err(EndpointProbeError::Connect(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "backend endpoint probe connect timed out after the probe deadline \
                 (endpoint {}): the listener exists but never completed the connection",
                endpoint.path
            ),
        ))),
    }
}

fn write_probe_frame_with_deadline(
    stream: &mut interprocess::local_socket::Stream,
    body: &[u8],
    deadline: Instant,
) -> Result<(), EndpointProbeError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(EndpointProbeError::FrameTooLarge {
            body_length: body.len(),
            cap: MAX_FRAME_BYTES,
        });
    }
    let mut wire = Vec::with_capacity(1 + 4 + body.len());
    wire.push(ENVELOPE_VERSION);
    wire.extend_from_slice(&(body.len() as u32).to_le_bytes());
    wire.extend_from_slice(body);
    write_all_with_deadline(stream, &wire, deadline)?;
    flush_with_deadline(stream, deadline)
}

fn read_probe_frame_with_deadline(
    stream: &mut interprocess::local_socket::Stream,
    deadline: Instant,
) -> Result<Vec<u8>, EndpointProbeError> {
    parse_probe_frame(|buf| read_exact_with_deadline(stream, buf, deadline))
}

/// Read one length-prefixed probe frame from an in-memory or blocking reader.
///
/// This drives the same byte-level parser as the nonblocking
/// deadline-enforcing read used by [`probe_endpoint_response`]; it is exposed
/// so fuzzing and tests can feed the framing logic from a
/// [`std::io::Cursor`]. EOF surfaces as [`EndpointProbeError::Io`] instead of
/// being retried against a deadline.
pub fn read_probe_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, EndpointProbeError> {
    parse_probe_frame(|buf| reader.read_exact(buf).map_err(EndpointProbeError::Io))
}

/// Pure byte-level probe frame parse shared by the deadline-enforcing read
/// and the fuzzing seam: a 1-byte envelope version ([`ENVELOPE_VERSION`]), a
/// little-endian `u32` body length capped at [`MAX_FRAME_BYTES`], then the
/// body bytes.
fn parse_probe_frame(
    mut read_exact: impl FnMut(&mut [u8]) -> Result<(), EndpointProbeError>,
) -> Result<Vec<u8>, EndpointProbeError> {
    let mut version = [0_u8; 1];
    read_exact(&mut version)?;
    if version[0] != ENVELOPE_VERSION {
        return Err(EndpointProbeError::UnsupportedFramingVersion {
            got: version[0],
            expected: ENVELOPE_VERSION,
        });
    }

    let mut len = [0_u8; 4];
    read_exact(&mut len)?;
    let body_length = u32::from_le_bytes(len) as usize;
    if body_length > MAX_FRAME_BYTES {
        return Err(EndpointProbeError::FrameTooLarge {
            body_length,
            cap: MAX_FRAME_BYTES,
        });
    }

    let mut body = vec![0_u8; body_length];
    if body_length > 0 {
        read_exact(&mut body)?;
    }
    Ok(body)
}

fn write_all_with_deadline<W: Write>(
    writer: &mut W,
    mut buf: &[u8],
    deadline: Instant,
) -> Result<(), EndpointProbeError> {
    while !buf.is_empty() {
        match writer.write(buf) {
            Ok(0) => {
                return Err(EndpointProbeError::Io(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "endpoint probe write returned zero bytes",
                )));
            }
            Ok(written) => buf = &buf[written..],
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) => return Err(EndpointProbeError::Io(err)),
        }
    }
    Ok(())
}

fn read_exact_with_deadline<R: Read>(
    reader: &mut R,
    mut buf: &mut [u8],
    deadline: Instant,
) -> Result<(), EndpointProbeError> {
    while !buf.is_empty() {
        match reader.read(buf) {
            Ok(0) => wait_for_io(deadline)?,
            Ok(read) => {
                let tmp = buf;
                buf = &mut tmp[read..];
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) => return Err(EndpointProbeError::Io(err)),
        }
    }
    Ok(())
}

fn flush_with_deadline<W: Write>(
    writer: &mut W,
    deadline: Instant,
) -> Result<(), EndpointProbeError> {
    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => wait_for_io(deadline)?,
            Err(err) => return Err(EndpointProbeError::Io(err)),
        }
    }
}

fn wait_for_io(deadline: Instant) -> Result<(), EndpointProbeError> {
    if Instant::now() >= deadline {
        return Err(EndpointProbeError::Timeout);
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    thread::sleep(remaining.min(NONBLOCKING_POLL_INTERVAL));
    Ok(())
}

fn endpoint_name(path: &str) -> io::Result<interprocess::local_socket::Name<'_>> {
    #[cfg(unix)]
    {
        use interprocess::local_socket::GenericFilePath;
        path.to_fs_name::<GenericFilePath>()
    }

    #[cfg(windows)]
    {
        use interprocess::local_socket::GenericNamespaced;
        path.to_ns_name::<GenericNamespaced>()
    }
}
