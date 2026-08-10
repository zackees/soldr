//! v1 broker wire framing — `[u8 framing_version][u32 LE body_length][prost body]`.
//!
//! ## Frozen-forever
//!
//! This module implements THE truly-frozen-forever invariant of v1
//! per #228 "Frozen-forever commitments": the framing byte is `1`,
//! the body length is a little-endian `u32`, and the body is a prost
//! payload. Any future protocol version is signalled by changing the
//! framing byte, which lets a v1 broker recognize a v2 client and
//! return `Refused{ERROR_VERSION_UNSUPPORTED}` instead of decoding
//! garbage.
//!
//! ## Sizes
//!
//! - [`MAX_FRAME_BYTES`] caps an arbitrary frame at 16 MiB. Larger
//!   frames cause the broker to disconnect.
//! - [`MAX_HELLO_BYTES`] caps the initial Hello envelope at 64 KiB.
//!   Larger Hello frames cause the broker to return `Refused` and
//!   close. Hello-specific reads should pass [`MAX_HELLO_BYTES`] to
//!   [`read_frame_with_cap`].
//!
//! ## Sync, not async
//!
//! Phase 1 ships a synchronous `std::io::{Read, Write}` implementation
//! because the `client` cargo feature does not pull in tokio. The
//! broker server (Phase 4) runs under `feature = "daemon"` (which
//! does include tokio) and can wrap a `TcpStream`/`NamedPipeServer`
//! either through `tokio::io::sync_bridge` or by re-implementing the
//! wire layout on `AsyncRead`/`AsyncWrite`. The wire format is the
//! same; only the surface API differs.

use std::io::{self, Read, Write};

use crate::broker::{FRAMING_VERSION_V1, MAX_FRAME_SIZE_BYTES, MAX_HELLO_SIZE_BYTES};

/// Framing byte for v1. Alias of [`crate::broker::FRAMING_VERSION_V1`]
/// to match the name used in the #228/#230 specs verbatim.
pub const ENVELOPE_VERSION: u8 = FRAMING_VERSION_V1;

/// Default per-frame size cap (16 MiB). Alias of
/// [`crate::broker::MAX_FRAME_SIZE_BYTES`].
pub const MAX_FRAME_BYTES: usize = MAX_FRAME_SIZE_BYTES;

/// Hello-envelope size cap (64 KiB). Alias of
/// [`crate::broker::MAX_HELLO_SIZE_BYTES`].
pub const MAX_HELLO_BYTES: usize = MAX_HELLO_SIZE_BYTES;

/// Errors produced by [`read_frame`]/[`write_frame`].
///
/// The error variants are deliberately distinct from `io::Error` so
/// callers can map them onto the broker's wire-level error codes (e.g.
/// `Refused{ERROR_VERSION_UNSUPPORTED}` for an
/// [`FramingError::UnsupportedFramingVersion`]).
#[derive(Debug, thiserror::Error)]
pub enum FramingError {
    /// Peer's framing byte did not match [`ENVELOPE_VERSION`].
    ///
    /// Per #228, the v1 broker writes `Refused{ERROR_VERSION_UNSUPPORTED}`
    /// to the wire and closes the connection on this error.
    #[error("unsupported framing version: got {got}, expected {expected}")]
    UnsupportedFramingVersion {
        /// The framing byte the peer actually sent.
        got: u8,
        /// The framing byte we expected (always
        /// [`ENVELOPE_VERSION`] in v1).
        expected: u8,
    },

    /// Body length exceeds the configured per-frame cap.
    ///
    /// The broker disconnects on this error per the wire-level
    /// commitments in #228. Callers should not attempt to drain the
    /// peer's payload — the socket is no longer in a known state.
    #[error("frame body too large: {body_length} bytes exceeds cap {cap}")]
    FrameTooLarge {
        /// The length the peer claimed in the 4-byte LE header.
        body_length: usize,
        /// The cap the caller passed to [`read_frame_with_cap`].
        cap: usize,
    },

    /// The underlying stream closed before the full frame arrived.
    #[error("unexpected EOF while reading frame ({context})")]
    UnexpectedEof {
        /// Which part of the frame we were reading (e.g. "framing byte").
        context: &'static str,
    },

    /// Raw I/O error from the underlying stream.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The frame body was not a valid prost `Frame` message.
    ///
    /// Produced by the buffer-level
    /// [`try_decode_framed`](super::frame_ext::try_decode_framed)
    /// codec (#412); the stream-level [`read_frame`] returns raw body
    /// bytes and leaves prost decoding to the caller.
    #[error("failed to decode Frame body: {0}")]
    Decode(#[from] prost::DecodeError),
}

/// Read one v1 frame from `reader` with the default 16 MiB cap.
///
/// Equivalent to `read_frame_with_cap(reader, MAX_FRAME_BYTES)`. Use
/// [`read_frame_with_cap`] with [`MAX_HELLO_BYTES`] for the initial
/// Hello envelope.
///
/// # Errors
///
/// - [`FramingError::UnsupportedFramingVersion`] if the leading byte
///   is not [`ENVELOPE_VERSION`]. The connection should be closed
///   after writing `Refused{ERROR_VERSION_UNSUPPORTED}`.
/// - [`FramingError::FrameTooLarge`] if the body-length header
///   exceeds the cap. The connection should be closed.
/// - [`FramingError::UnexpectedEof`] if the stream returned EOF before
///   all expected bytes arrived.
/// - [`FramingError::Io`] for any other I/O error.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Vec<u8>, FramingError> {
    read_frame_with_cap(reader, MAX_FRAME_BYTES)
}

/// Read one v1 frame from `reader`, rejecting bodies larger than `max_bytes`.
///
/// Pass [`MAX_HELLO_BYTES`] (64 KiB) for the initial Hello envelope,
/// and [`MAX_FRAME_BYTES`] (16 MiB) — the default in
/// [`read_frame`] — for all subsequent frames.
pub fn read_frame_with_cap<R: Read>(
    reader: &mut R,
    max_bytes: usize,
) -> Result<Vec<u8>, FramingError> {
    // Step 1: framing version byte.
    let mut version_buf = [0u8; 1];
    read_exact_or_eof(reader, &mut version_buf, "framing byte")?;
    let version = version_buf[0];
    if version != ENVELOPE_VERSION {
        return Err(FramingError::UnsupportedFramingVersion {
            got: version,
            expected: ENVELOPE_VERSION,
        });
    }

    // Step 2: body length (4 bytes, little-endian).
    let mut len_buf = [0u8; 4];
    read_exact_or_eof(reader, &mut len_buf, "body length header")?;
    let body_length = u32::from_le_bytes(len_buf) as usize;

    // Step 3: enforce size cap *before* allocating.
    if body_length > max_bytes {
        return Err(FramingError::FrameTooLarge {
            body_length,
            cap: max_bytes,
        });
    }

    // Step 4: read the body. A zero-length body is legal — Frame
    // messages with an empty payload are explicitly allowed by the
    // v1 schema (e.g. heartbeat-style probes).
    let mut body = vec![0u8; body_length];
    if body_length > 0 {
        read_exact_or_eof(reader, &mut body, "frame body")?;
    }
    Ok(body)
}

/// Write one v1 frame to `writer`.
///
/// The frame is laid out as `[u8 framing_version=1][u32 LE
/// body_length][body]`. Returns the number of bytes written on
/// success (5 + body.len()).
///
/// # Errors
///
/// - [`FramingError::FrameTooLarge`] if `body.len()` exceeds
///   [`MAX_FRAME_BYTES`]. The caller must not exceed
///   [`MAX_HELLO_BYTES`] for Hello frames; this guard catches only
///   the absolute ceiling.
/// - [`FramingError::Io`] for any other I/O error.
pub fn write_frame<W: Write>(writer: &mut W, body: &[u8]) -> Result<usize, FramingError> {
    if body.len() > MAX_FRAME_BYTES {
        return Err(FramingError::FrameTooLarge {
            body_length: body.len(),
            cap: MAX_FRAME_BYTES,
        });
    }

    // u32 LE body length — `body.len()` fits in u32 because the cap
    // (16 MiB) is well under u32::MAX.
    let body_len_u32 = body.len() as u32;
    let header: [u8; 5] = [
        ENVELOPE_VERSION,
        (body_len_u32 & 0xFF) as u8,
        ((body_len_u32 >> 8) & 0xFF) as u8,
        ((body_len_u32 >> 16) & 0xFF) as u8,
        ((body_len_u32 >> 24) & 0xFF) as u8,
    ];

    writer.write_all(&header)?;
    if !body.is_empty() {
        writer.write_all(body)?;
    }
    writer.flush()?;
    Ok(header.len() + body.len())
}

fn read_exact_or_eof<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    context: &'static str,
) -> Result<(), FramingError> {
    match reader.read_exact(buf) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            Err(FramingError::UnexpectedEof { context })
        }
        Err(err) => Err(FramingError::Io(err)),
    }
}
