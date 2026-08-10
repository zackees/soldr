//! Ergonomic constructors and buffer-level codecs for the v1 `Frame`
//! envelope (#412).
//!
//! The broker v1 post-mortem found every consumer hand-building
//! `Frame { envelope_version: 1, kind, payload_protocol, payload,
//! request_id, payload_encoding: None, deadline_unix_ms: 0,
//! traceparent: "", tracestate: "" }` plus the outer
//! `[u8 1][u32 LE len]` header, and re-deriving the
//! peek-without-consume decode against a growable read buffer. This
//! module owns those four pieces:
//!
//! - [`Frame::request`] / [`Frame::response_to`] — envelope
//!   construction with correct v1 defaults.
//! - [`encode_framed`] — one `Frame` to complete wire bytes
//!   (`[1][len][prost]`).
//! - [`try_decode_framed`] — incremental decode from a byte buffer,
//!   returning how many bytes the caller must consume.
//! - [`Endpoint::windows_pipe`] / [`Endpoint::unix_socket`] — endpoint
//!   identities that respect the platform naming rules (notably: on
//!   Windows the path is the **bare** pipe name, never
//!   `\\.\pipe\`-prefixed, because running-process resolves endpoint
//!   paths through interprocess's `GenericNamespaced` name type which
//!   prepends the prefix itself).
//!
//! The sync-stream helpers stay in [`super::framing`]; this module is
//! the buffer-level twin for consumers with their own buffered I/O
//! (async daemons accumulating into a `BytesMut`-style buffer).

use prost::Message;

use crate::broker::protocol::{
    Endpoint, Frame, FrameKind, FramingError, PayloadEncoding, ENVELOPE_VERSION, MAX_FRAME_BYTES,
    PROTOCOL_VERSION,
};

/// Length of the outer wire header: `[u8 framing_version][u32 LE body_len]`.
pub const FRAME_HEADER_BYTES: usize = 5;

impl Frame {
    /// Build a v1 request frame with correct envelope defaults.
    ///
    /// Sets `envelope_version` to [`PROTOCOL_VERSION`], `kind` to
    /// `REQUEST`, `payload_encoding` to `NONE`, no deadline, and empty
    /// trace context. Callers correlate request/response pairs through
    /// `request_id` (see [`Frame::with_request_id`]).
    ///
    /// ```
    /// use running_process::broker::protocol::Frame;
    ///
    /// let frame = Frame::request(0x7A63, b"payload".to_vec()).with_request_id(7);
    /// assert_eq!(frame.envelope_version, 1);
    /// assert_eq!(frame.request_id, 7);
    /// ```
    pub fn request(payload_protocol: u32, payload: Vec<u8>) -> Self {
        Self {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Request as i32,
            payload_protocol,
            payload,
            request_id: 0,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        }
    }

    /// Build the v1 response frame for `request`.
    ///
    /// Echoes the request's `payload_protocol`, `request_id`, and trace
    /// context, sets `kind` to `RESPONSE`, and carries `payload`.
    ///
    /// ```
    /// use running_process::broker::protocol::Frame;
    ///
    /// let request = Frame::request(0x7A63, b"ping".to_vec()).with_request_id(9);
    /// let response = Frame::response_to(&request, b"pong".to_vec());
    /// assert_eq!(response.request_id, 9);
    /// assert_eq!(response.payload_protocol, 0x7A63);
    /// ```
    pub fn response_to(request: &Self, payload: Vec<u8>) -> Self {
        Self {
            envelope_version: PROTOCOL_VERSION,
            kind: FrameKind::Response as i32,
            payload_protocol: request.payload_protocol,
            payload,
            request_id: request.request_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: request.traceparent.clone(),
            tracestate: request.tracestate.clone(),
        }
    }

    /// Set the correlation `request_id` (builder style).
    #[must_use]
    pub fn with_request_id(mut self, request_id: u64) -> Self {
        self.request_id = request_id;
        self
    }
}

/// Encode one `Frame` into complete wire bytes:
/// `[u8 framing_version=1][u32 LE body_len][prost Frame]`.
///
/// # Errors
///
/// [`FramingError::FrameTooLarge`] when the encoded body exceeds
/// [`MAX_FRAME_BYTES`].
pub fn encode_framed(frame: &Frame) -> Result<Vec<u8>, FramingError> {
    let body_len = frame.encoded_len();
    if body_len > MAX_FRAME_BYTES {
        return Err(FramingError::FrameTooLarge {
            body_length: body_len,
            cap: MAX_FRAME_BYTES,
        });
    }
    let mut wire = Vec::with_capacity(FRAME_HEADER_BYTES + body_len);
    wire.push(ENVELOPE_VERSION);
    wire.extend_from_slice(&(body_len as u32).to_le_bytes());
    frame
        .encode(&mut wire)
        .expect("prost encoding into Vec cannot fail because Vec writes are infallible");
    Ok(wire)
}

/// One frame decoded from a byte buffer by [`try_decode_framed`].
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedFramed {
    /// The decoded frame.
    pub frame: Frame,
    /// Total wire bytes the frame occupied (header + body). The caller
    /// must consume exactly this many bytes from the front of its buffer.
    pub consumed: usize,
}

/// Incrementally decode one `Frame` from the front of `buf`.
///
/// Returns `Ok(None)` when the buffer does not yet hold a complete
/// frame (read more bytes and retry); the buffer is never logically
/// consumed — on `Ok(Some(decoded))` the caller advances its buffer by
/// `decoded.consumed` bytes.
///
/// # Errors
///
/// - [`FramingError::UnsupportedFramingVersion`] when the leading byte
///   is not [`ENVELOPE_VERSION`]. Consumers multiplexing a legacy wire
///   on the same endpoint should classify the buffer first (see
///   [`crate::broker::backend_sdk::BackendEndpointMux`]).
/// - [`FramingError::FrameTooLarge`] when the advertised body length
///   exceeds [`MAX_FRAME_BYTES`].
/// - [`FramingError::Decode`] when the body bytes are not a valid
///   prost `Frame`.
pub fn try_decode_framed(buf: &[u8]) -> Result<Option<DecodedFramed>, FramingError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] != ENVELOPE_VERSION {
        return Err(FramingError::UnsupportedFramingVersion {
            got: buf[0],
            expected: ENVELOPE_VERSION,
        });
    }
    if buf.len() < FRAME_HEADER_BYTES {
        return Ok(None);
    }
    let body_len = u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
    if body_len > MAX_FRAME_BYTES {
        return Err(FramingError::FrameTooLarge {
            body_length: body_len,
            cap: MAX_FRAME_BYTES,
        });
    }
    let total = FRAME_HEADER_BYTES + body_len;
    if buf.len() < total {
        return Ok(None);
    }
    let frame = Frame::decode(&buf[FRAME_HEADER_BYTES..total]).map_err(FramingError::Decode)?;
    Ok(Some(DecodedFramed {
        frame,
        consumed: total,
    }))
}

impl Endpoint {
    /// Build a Windows named-pipe endpoint identity from a **bare**
    /// pipe name.
    ///
    /// running-process resolves endpoint paths through interprocess's
    /// `GenericNamespaced` name type, which prepends `\\.\pipe\`
    /// itself. Passing an already-prefixed path silently addresses the
    /// wrong pipe, so this constructor rejects it.
    ///
    /// Available on every platform so cross-platform consumers can
    /// construct endpoint identities for manifests and diagnostics.
    ///
    /// ```
    /// use running_process::broker::protocol::Endpoint;
    ///
    /// let endpoint = Endpoint::windows_pipe("my-daemon", "my-daemon-pipe").unwrap();
    /// assert_eq!(endpoint.path, "my-daemon-pipe");
    /// assert!(Endpoint::windows_pipe("my-daemon", r"\\.\pipe\my-daemon-pipe").is_err());
    /// ```
    ///
    /// # Errors
    ///
    /// [`EndpointNameError::PrefixedPipeName`] when `pipe_name` starts
    /// with `\\.\pipe\` (or the forward-slash spelling), and
    /// [`EndpointNameError::Empty`] for an empty name.
    pub fn windows_pipe(
        namespace_id: impl Into<String>,
        pipe_name: impl Into<String>,
    ) -> Result<Self, EndpointNameError> {
        let pipe_name = pipe_name.into();
        if pipe_name.is_empty() {
            return Err(EndpointNameError::Empty);
        }
        let lowered = pipe_name.to_ascii_lowercase().replace('/', "\\");
        if lowered.starts_with("\\\\.\\pipe\\") {
            return Err(EndpointNameError::PrefixedPipeName { got: pipe_name });
        }
        Ok(Self {
            namespace_id: namespace_id.into(),
            path: pipe_name,
        })
    }

    /// Build a Unix-domain-socket endpoint identity from a filesystem
    /// path.
    ///
    /// Available on every platform so cross-platform consumers can
    /// construct endpoint identities for manifests and diagnostics.
    ///
    /// # Errors
    ///
    /// [`EndpointNameError::Empty`] for an empty path.
    pub fn unix_socket(
        namespace_id: impl Into<String>,
        socket_path: impl Into<String>,
    ) -> Result<Self, EndpointNameError> {
        let socket_path = socket_path.into();
        if socket_path.is_empty() {
            return Err(EndpointNameError::Empty);
        }
        Ok(Self {
            namespace_id: namespace_id.into(),
            path: socket_path,
        })
    }
}

/// Errors from the [`Endpoint`] smart constructors.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EndpointNameError {
    /// The endpoint name or path was empty.
    #[error("endpoint name must not be empty")]
    Empty,
    /// A Windows pipe name carried the `\\.\pipe\` prefix; endpoint
    /// paths must be the bare pipe name.
    #[error(
        "windows pipe name must be bare (no \\\\.\\pipe\\ prefix), got {got:?}: \
         running-process prepends the prefix when resolving the endpoint"
    )]
    PrefixedPipeName {
        /// The rejected, already-prefixed name.
        got: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_response_round_trip_through_buffer_codecs() {
        let request = Frame::request(0x7A63, b"ping".to_vec()).with_request_id(42);
        assert_eq!(request.envelope_version, PROTOCOL_VERSION);
        assert_eq!(FrameKind::try_from(request.kind), Ok(FrameKind::Request));
        assert_eq!(
            PayloadEncoding::try_from(request.payload_encoding),
            Ok(PayloadEncoding::None)
        );

        let response = Frame::response_to(&request, b"pong".to_vec());
        assert_eq!(FrameKind::try_from(response.kind), Ok(FrameKind::Response));
        assert_eq!(response.request_id, 42);
        assert_eq!(response.payload_protocol, 0x7A63);

        let wire = encode_framed(&request).expect("encode");
        assert_eq!(wire[0], ENVELOPE_VERSION);
        let decoded = try_decode_framed(&wire)
            .expect("decode")
            .expect("complete frame");
        assert_eq!(decoded.frame, request);
        assert_eq!(decoded.consumed, wire.len());
    }

    #[test]
    fn response_echoes_trace_context() {
        let mut request = Frame::request(0x7A63, Vec::new()).with_request_id(1);
        request.traceparent = "00-abc-def-01".to_owned();
        request.tracestate = "vendor=1".to_owned();
        let response = Frame::response_to(&request, Vec::new());
        assert_eq!(response.traceparent, request.traceparent);
        assert_eq!(response.tracestate, request.tracestate);
    }

    #[test]
    fn try_decode_framed_waits_for_complete_frames() {
        let wire = encode_framed(&Frame::request(0x7001, b"abc".to_vec())).expect("encode");
        assert!(try_decode_framed(&[]).expect("empty").is_none());
        for cut in 1..wire.len() {
            assert!(
                try_decode_framed(&wire[..cut]).expect("partial").is_none(),
                "partial frame of {cut} bytes must not decode"
            );
        }
        // Trailing bytes after a complete frame are left for the next decode.
        let mut two = wire.clone();
        two.extend_from_slice(&wire);
        let first = try_decode_framed(&two).expect("decode").expect("complete");
        assert_eq!(first.consumed, wire.len());
    }

    #[test]
    fn try_decode_framed_rejects_foreign_version_and_oversize() {
        assert!(matches!(
            try_decode_framed(&[2, 0, 0, 0, 0]),
            Err(FramingError::UnsupportedFramingVersion { got: 2, .. })
        ));
        let mut oversize = vec![ENVELOPE_VERSION];
        oversize.extend_from_slice(&(MAX_FRAME_BYTES as u32 + 1).to_le_bytes());
        assert!(matches!(
            try_decode_framed(&oversize),
            Err(FramingError::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn endpoint_constructors_enforce_naming_rules() {
        let pipe = Endpoint::windows_pipe("svc", "svc-pipe").expect("bare name");
        assert_eq!(pipe.namespace_id, "svc");
        assert_eq!(pipe.path, "svc-pipe");

        assert_eq!(
            Endpoint::windows_pipe("svc", r"\\.\pipe\svc-pipe"),
            Err(EndpointNameError::PrefixedPipeName {
                got: r"\\.\pipe\svc-pipe".to_owned()
            })
        );
        assert_eq!(
            Endpoint::windows_pipe("svc", "//./pipe/svc-pipe"),
            Err(EndpointNameError::PrefixedPipeName {
                got: "//./pipe/svc-pipe".to_owned()
            })
        );
        assert_eq!(
            Endpoint::windows_pipe("svc", ""),
            Err(EndpointNameError::Empty)
        );

        let sock = Endpoint::unix_socket("svc", "/tmp/svc.sock").expect("path");
        assert_eq!(sock.path, "/tmp/svc.sock");
        assert_eq!(
            Endpoint::unix_socket("svc", ""),
            Err(EndpointNameError::Empty)
        );
    }
}
