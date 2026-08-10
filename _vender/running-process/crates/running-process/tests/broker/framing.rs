//! Phase 1 of #228 (issue #230) — coverage for
//! `crate::broker::protocol::framing`.
//!
//! All round-trips run over an in-memory buffer because the framing
//! API is generic over `std::io::{Read, Write}`. Real socket coverage
//! lives in the Phase 4 broker tests when a server actually exists.

#![cfg(feature = "client")]

use std::io::Cursor;

use running_process::broker::protocol::framing::{
    read_frame, read_frame_with_cap, write_frame, FramingError, ENVELOPE_VERSION, MAX_FRAME_BYTES,
    MAX_HELLO_BYTES,
};

#[test]
fn happy_path_roundtrip() {
    let payload: Vec<u8> = (0..1024u16).map(|i| (i & 0xFF) as u8).collect();
    let mut buf = Vec::new();
    let written = write_frame(&mut buf, &payload).expect("write");
    assert_eq!(written, 5 + payload.len(), "frame is 5-byte header + body");
    assert_eq!(
        buf[0], ENVELOPE_VERSION,
        "first byte must be framing version"
    );

    let mut cursor = Cursor::new(buf);
    let body = read_frame(&mut cursor).expect("read");
    assert_eq!(body, payload);
}

#[test]
fn empty_body_roundtrip() {
    let mut buf = Vec::new();
    write_frame(&mut buf, &[]).expect("write");
    // Header is `[1, 0, 0, 0, 0]` — 5 bytes total.
    assert_eq!(buf.len(), 5);
    assert_eq!(buf[0], ENVELOPE_VERSION);
    assert_eq!(&buf[1..], &[0, 0, 0, 0]);
    let mut cursor = Cursor::new(buf);
    let body = read_frame(&mut cursor).expect("read");
    assert!(body.is_empty());
}

#[test]
fn framing_version_mismatch_returns_unsupported() {
    // Hand-craft a frame with a bogus version byte.
    let bad: Vec<u8> = vec![2, 0, 0, 0, 0];
    let mut cursor = Cursor::new(bad);
    let err = read_frame(&mut cursor).unwrap_err();
    match err {
        FramingError::UnsupportedFramingVersion { got, expected } => {
            assert_eq!(got, 2);
            assert_eq!(expected, ENVELOPE_VERSION);
        }
        other => panic!("expected UnsupportedFramingVersion, got {other:?}"),
    }
}

#[test]
fn oversize_returns_frame_too_large() {
    // Header claims a body 1 byte larger than the cap. We don't have
    // to actually supply that many bytes; the size check happens
    // before any allocation.
    let cap = 1024usize;
    let claimed = (cap as u32) + 1;
    let mut frame: Vec<u8> = Vec::new();
    frame.push(ENVELOPE_VERSION);
    frame.extend_from_slice(&claimed.to_le_bytes());
    let mut cursor = Cursor::new(frame);
    let err = read_frame_with_cap(&mut cursor, cap).unwrap_err();
    match err {
        FramingError::FrameTooLarge {
            body_length,
            cap: c,
        } => {
            assert_eq!(body_length, (cap + 1));
            assert_eq!(c, cap);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[test]
fn short_read_on_version_byte_errors_cleanly() {
    let empty: Vec<u8> = Vec::new();
    let mut cursor = Cursor::new(empty);
    let err = read_frame(&mut cursor).unwrap_err();
    match err {
        FramingError::UnexpectedEof { context } => {
            assert!(context.contains("framing"), "context: {context}");
        }
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn short_read_on_length_header_errors_cleanly() {
    // Only the version byte — no length header.
    let truncated: Vec<u8> = vec![ENVELOPE_VERSION];
    let mut cursor = Cursor::new(truncated);
    let err = read_frame(&mut cursor).unwrap_err();
    match err {
        FramingError::UnexpectedEof { context } => {
            assert!(context.contains("length"), "context: {context}");
        }
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn short_read_on_body_errors_cleanly() {
    // Claim 10 bytes of body; supply only 4.
    let mut frame: Vec<u8> = Vec::new();
    frame.push(ENVELOPE_VERSION);
    frame.extend_from_slice(&10u32.to_le_bytes());
    frame.extend_from_slice(&[0xAB, 0xCD, 0xEF, 0x01]);
    let mut cursor = Cursor::new(frame);
    let err = read_frame(&mut cursor).unwrap_err();
    match err {
        FramingError::UnexpectedEof { context } => {
            assert!(context.contains("body"), "context: {context}");
        }
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

#[test]
fn write_frame_rejects_oversize_body() {
    // Use a sparse vector so we don't actually allocate 16+ MiB.
    let mut huge = Vec::with_capacity(0);
    huge.resize(MAX_FRAME_BYTES + 1, 0);
    let mut buf = Vec::new();
    let err = write_frame(&mut buf, &huge).unwrap_err();
    match err {
        FramingError::FrameTooLarge { body_length, cap } => {
            assert_eq!(body_length, MAX_FRAME_BYTES + 1);
            assert_eq!(cap, MAX_FRAME_BYTES);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[test]
fn hello_cap_enforced_separately() {
    // A 64KiB+1 payload is fine under MAX_FRAME_BYTES but must be
    // rejected when the caller is reading a Hello and passes
    // MAX_HELLO_BYTES as the cap.
    let body_len = MAX_HELLO_BYTES + 1;
    let mut frame: Vec<u8> = Vec::new();
    frame.push(ENVELOPE_VERSION);
    frame.extend_from_slice(&(body_len as u32).to_le_bytes());
    // We don't need to supply the body — the cap check fires first.
    let mut cursor = Cursor::new(frame);
    let err = read_frame_with_cap(&mut cursor, MAX_HELLO_BYTES).unwrap_err();
    match err {
        FramingError::FrameTooLarge { body_length, cap } => {
            assert_eq!(body_length, MAX_HELLO_BYTES + 1);
            assert_eq!(cap, MAX_HELLO_BYTES);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[test]
fn multiple_frames_back_to_back() {
    // Write three frames in sequence, read them back in order.
    let mut buf = Vec::new();
    write_frame(&mut buf, b"first").unwrap();
    write_frame(&mut buf, b"second").unwrap();
    write_frame(&mut buf, b"third").unwrap();

    let mut cursor = Cursor::new(buf);
    assert_eq!(read_frame(&mut cursor).unwrap(), b"first");
    assert_eq!(read_frame(&mut cursor).unwrap(), b"second");
    assert_eq!(read_frame(&mut cursor).unwrap(), b"third");
}
