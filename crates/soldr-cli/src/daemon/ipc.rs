//! Frame encoding/decoding for the daemon protocol. The sync helpers
//! are used by the wrapper-side client (no tokio runtime on the hot
//! path); the async helpers are used by the server.

use crate::daemon::protocol::{MAX_BODY_BYTES, PROTOCOL_VERSION};
use serde::{de::DeserializeOwned, Serialize};
use std::io::{self, Read, Write};

const HEADER_BYTES: usize = 8;

fn encoding_error(e: bincode::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e)
}

pub fn encode_frame<T: Serialize>(msg: &T) -> io::Result<Vec<u8>> {
    let body = bincode::serialize(msg).map_err(encoding_error)?;
    if body.len() as u32 > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body exceeds MAX_BODY_BYTES",
        ));
    }
    let mut buf = Vec::with_capacity(HEADER_BYTES + body.len());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    buf.extend_from_slice(&body);
    Ok(buf)
}

pub fn write_frame_sync<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let frame = encode_frame(msg)?;
    w.write_all(&frame)?;
    w.flush()?;
    Ok(())
}

pub fn read_frame_sync<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<T> {
    let mut header = [0u8; HEADER_BYTES];
    r.read_exact(&mut header)?;
    let body_len = u32::from_le_bytes(header[..4].try_into().expect("4 bytes"));
    let version = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
    if version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("protocol version mismatch: peer={version}, ours={PROTOCOL_VERSION}"),
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
    bincode::deserialize(&body).map_err(encoding_error)
}

pub async fn write_frame_async<W, T>(w: &mut W, msg: &T) -> io::Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
    T: Serialize,
{
    use tokio::io::AsyncWriteExt;
    let frame = encode_frame(msg)?;
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_frame_async<R, T>(r: &mut R) -> io::Result<T>
where
    R: tokio::io::AsyncRead + Unpin,
    T: DeserializeOwned,
{
    use tokio::io::AsyncReadExt;
    let mut header = [0u8; HEADER_BYTES];
    r.read_exact(&mut header).await?;
    let body_len = u32::from_le_bytes(header[..4].try_into().expect("4 bytes"));
    let version = u32::from_le_bytes(header[4..].try_into().expect("4 bytes"));
    if version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("protocol version mismatch: peer={version}, ours={PROTOCOL_VERSION}"),
        ));
    }
    if body_len > MAX_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame body exceeds MAX_BODY_BYTES",
        ));
    }
    let mut body = vec![0u8; body_len as usize];
    r.read_exact(&mut body).await?;
    bincode::deserialize(&body).map_err(encoding_error)
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
    fn oversize_body_is_rejected_on_encode() {
        let blob = "x".repeat((MAX_BODY_BYTES + 1) as usize);
        let msg = Request::RecordTargetTouch {
            path: blob,
            unix_seconds: 0,
        };
        let err = encode_frame(&msg).expect_err("too large");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
