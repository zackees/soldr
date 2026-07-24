//! Frame encoding/decoding for the daemon protocol. The sync helpers
//! are used by the wrapper-side client (no tokio runtime on the hot
//! path); the async helpers are used by the server.
//!
//! ## Wire shape (issue #580)
//!
//! The header is unchanged from prior versions:
//!
//! ```text
//! [u32 LE body_len][u32 LE protocol_version][body]
//! ```
//!
//! What changed in v5 is the **body**: it is now a prost-encoded
//! `WireRequest` / `WireResponse` (see [`crate::daemon::wire`])
//! instead of a bincode blob. The header version-check rejects
//! cross-version traffic cleanly, so a v4 client can never silently
//! mis-decode a v5 daemon's reply (or vice versa).

use crate::daemon::protocol::{
    Request, Response, WireDecodeError, MAX_BODY_BYTES, PROTOCOL_VERSION,
};
use crate::daemon::wire;
use std::io::{self, Read, Write};
use std::time::Duration;

const HEADER_BYTES: usize = 8;
/// Reads on the daemon side (a `RecordTargetTouch` or similar) used to
/// finish in milliseconds, so a 30 s cap was generous. With the
/// `Request::Compile` verb added in #977 Phase 5 / #980 L1 the wrapper
/// holds the daemon's IPC channel open for the lifetime of a rustc
/// invocation, which may take minutes for a large release crate. Bumped
/// to match `client::COMPILE_REPLY_TIMEOUT` so the same 30 minute
/// budget governs both directions; the wrapper's own
/// `submit_request_with_timeout` is the steady-state ceiling.
const ASYNC_FRAME_READ_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const ASYNC_FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

fn decode_error(e: WireDecodeError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

fn timed_out(operation: &str, duration: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!("{operation} timed out after {}s", duration.as_secs()),
    )
}

/// Abstracts over Request/Response so encode/read helpers don't have
/// to be split into four functions per direction. Each impl plugs in
/// the appropriate prost encode/decode pair from [`crate::daemon::wire`].
pub trait WireBody: Sized {
    fn encode_wire(&self) -> Vec<u8>;
    fn decode_wire(bytes: &[u8]) -> Result<Self, WireDecodeError>;
}

impl WireBody for Request {
    fn encode_wire(&self) -> Vec<u8> {
        wire::encode_request(self)
    }
    fn decode_wire(bytes: &[u8]) -> Result<Self, WireDecodeError> {
        wire::decode_request(bytes)
    }
}

impl WireBody for Response {
    fn encode_wire(&self) -> Vec<u8> {
        wire::encode_response(self)
    }
    fn decode_wire(bytes: &[u8]) -> Result<Self, WireDecodeError> {
        wire::decode_response(bytes)
    }
}

pub fn encode_frame<T: WireBody>(msg: &T) -> io::Result<Vec<u8>> {
    encode_frame_for_version(msg, PROTOCOL_VERSION)
}

pub(crate) fn encode_frame_for_version<T: WireBody>(
    msg: &T,
    protocol_version: u32,
) -> io::Result<Vec<u8>> {
    let body = msg.encode_wire();
    if body.len() as u32 > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body exceeds MAX_BODY_BYTES",
        ));
    }
    let mut buf = Vec::with_capacity(HEADER_BYTES + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&protocol_version.to_le_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

pub fn write_frame_sync<W: Write, T: WireBody>(w: &mut W, msg: &T) -> io::Result<()> {
    write_frame_sync_for_version(w, msg, PROTOCOL_VERSION)
}

pub(crate) fn write_frame_sync_for_version<W: Write, T: WireBody>(
    w: &mut W,
    msg: &T,
    protocol_version: u32,
) -> io::Result<()> {
    let frame = encode_frame_for_version(msg, protocol_version)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame_sync<R: Read, T: WireBody>(r: &mut R) -> io::Result<T> {
    read_frame_sync_for_version(r, PROTOCOL_VERSION)
}

pub(crate) fn read_frame_sync_for_version<R: Read, T: WireBody>(
    r: &mut R,
    expected_version: u32,
) -> io::Result<T> {
    let mut header = [0u8; HEADER_BYTES];
    r.read_exact(&mut header)?;
    let body_len = u32::from_le_bytes(header[..4].try_into().expect("4 bytes"));
    let version = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
    if version != expected_version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("protocol version mismatch: peer={version}, expected={expected_version}"),
        ));
    }
    if body_len > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body exceeds MAX_BODY_BYTES",
        ));
    }
    let mut body = vec![0u8; body_len as usize];
    r.read_exact(&mut body)?;
    T::decode_wire(&body).map_err(decode_error)
}

pub async fn write_frame_async<W, T>(w: &mut W, msg: &T) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: WireBody,
{
    write_frame_async_for_version(w, msg, PROTOCOL_VERSION).await
}

pub(crate) async fn write_frame_async_for_version<W, T>(
    w: &mut W,
    msg: &T,
    protocol_version: u32,
) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: WireBody,
{
    use tokio::io::AsyncWriteExt;
    use tokio::time::timeout;
    let frame = encode_frame_for_version(msg, protocol_version)?;
    timeout(ASYNC_FRAME_WRITE_TIMEOUT, w.write_all(&frame))
        .await
        .map_err(|_| timed_out("daemon IPC frame write", ASYNC_FRAME_WRITE_TIMEOUT))??;
    timeout(ASYNC_FRAME_WRITE_TIMEOUT, w.flush())
        .await
        .map_err(|_| timed_out("daemon IPC frame flush", ASYNC_FRAME_WRITE_TIMEOUT))??;
    Ok(())
}

pub async fn read_frame_async<R, T>(r: &mut R) -> io::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: WireBody,
{
    read_frame_async_for_version(r, PROTOCOL_VERSION).await
}

pub(crate) async fn read_frame_async_for_version<R, T>(
    r: &mut R,
    expected_version: u32,
) -> io::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: WireBody,
{
    read_frame_async_with_prefix_for_version(r, &[], expected_version).await
}

pub async fn read_frame_async_with_prefix<R, T>(r: &mut R, prefix: &[u8]) -> io::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: WireBody,
{
    read_frame_async_with_prefix_for_version(r, prefix, PROTOCOL_VERSION).await
}

async fn read_frame_async_with_prefix_for_version<R, T>(
    r: &mut R,
    prefix: &[u8],
    expected_version: u32,
) -> io::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: WireBody,
{
    use tokio::io::AsyncReadExt;
    use tokio::time::timeout;
    if prefix.len() > HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame prefix exceeds header length",
        ));
    }
    let mut header = [0u8; HEADER_BYTES];
    header[..prefix.len()].copy_from_slice(prefix);
    if prefix.len() < HEADER_BYTES {
        timeout(
            ASYNC_FRAME_READ_TIMEOUT,
            r.read_exact(&mut header[prefix.len()..]),
        )
        .await
        .map_err(|_| timed_out("daemon IPC frame header read", ASYNC_FRAME_READ_TIMEOUT))??;
    }
    let body_len = u32::from_le_bytes(header[..4].try_into().expect("4 bytes"));
    let version = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
    if version != expected_version {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("protocol version mismatch: peer={version}, expected={expected_version}"),
        ));
    }
    if body_len > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body exceeds MAX_BODY_BYTES",
        ));
    }
    let mut body = vec![0u8; body_len as usize];
    timeout(ASYNC_FRAME_READ_TIMEOUT, r.read_exact(&mut body))
        .await
        .map_err(|_| timed_out("daemon IPC frame body read", ASYNC_FRAME_READ_TIMEOUT))??;
    T::decode_wire(&body).map_err(decode_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::Request;
    use std::io::Cursor;

    #[test]
    fn round_trip_record_target_touch() {
        let msg = Request::RecordTargetTouch {
            path: "/tmp/example/target".into(),
            unix_seconds: 1_700_000_000,
        };
        let frame = encode_frame(&msg).expect("encode");
        let mut cursor = Cursor::new(frame);
        let decoded: Request = read_frame_sync(&mut cursor).expect("decode");
        assert!(matches!(
            decoded,
            Request::RecordTargetTouch { ref path, unix_seconds }
                if path == "/tmp/example/target" && unix_seconds == 1_700_000_000
        ));
    }

    #[test]
    fn version_mismatch_is_invalid_data() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&4u32.to_le_bytes());
        bytes.extend_from_slice(&999u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        let mut cursor = Cursor::new(bytes);
        let err = read_frame_sync::<_, Request>(&mut cursor).expect_err("must error");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn explicit_protocol_version_round_trips_only_with_matching_reader() {
        let msg = Request::Status;
        let frame = encode_frame_for_version(&msg, 17).expect("encode legacy frame");

        let decoded: Request = read_frame_sync_for_version(&mut Cursor::new(frame.clone()), 17)
            .expect("matching legacy reader");
        assert!(matches!(decoded, Request::Status));

        let err = read_frame_sync::<_, Request>(&mut Cursor::new(frame))
            .expect_err("current reader must reject a legacy frame");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn oversize_body_is_rejected_on_encode() {
        // A very long path string blows past the body cap once prost
        // varint-length-prefixes the string field. The cap was bumped
        // from 64 KiB → 4 MiB in #977 Phase 5 to admit the Compile
        // verb's stdout/stderr payload; this test still proves the
        // cap is enforced regardless of its current numeric value.
        let blob = "x".repeat((MAX_BODY_BYTES + 1) as usize);
        let msg = Request::RecordTargetTouch {
            path: blob,
            unix_seconds: 0,
        };
        let err = encode_frame(&msg).expect_err("too large");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
