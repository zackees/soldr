//! Golden-bytes wire-format regression tests for the v1 broker (#377).
//!
//! These tests are the MECHANICAL ENFORCEMENT of the frozen-v1 policy
//! (`docs/v1-frozen-commitments.md`, `proto/broker_v1/*.proto`,
//! `src/broker/mod.rs`): the exact serialized bytes of every core
//! envelope message and the `[u8 version=1][u32 LE body_length][body]`
//! framing layout are pinned against checked-in byte literals. Any diff
//! to the golden bytes below is a wire-format break and MUST NOT be
//! merged for v1 — a change here means a prost upgrade or code edit
//! altered what goes on the wire, which would break cross-version
//! compatibility in the field even if every other test passes.
//!
//! The expected byte arrays were derived ONCE (by encoding the sample
//! values and pasting the output) and are now frozen; the tests never
//! re-derive expectations from the encoder under test.
//!
//! Determinism note: prost encodes fields in ascending field-number
//! order, so all scalar/bytes/string/repeated fields serialize
//! deterministically. Protobuf `map` fields (e.g. `Refused.details`)
//! are backed by `HashMap` and are NOT deterministic across entries —
//! golden samples must keep maps empty or use exactly one entry.

#![cfg(feature = "client")]

use std::collections::HashMap;
use std::io::Cursor;

use prost::Message;
use running_process::broker::protocol::framing::{
    read_frame, read_frame_with_cap, write_frame, FramingError, ENVELOPE_VERSION,
};
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, ErrorCode, Frame, FrameKind, HandoffAck, HandoffOffer,
    Hello, HelloReply, Negotiated, PayloadEncoding, Refused,
};
use running_process::broker::{FRAMING_VERSION_V1, MAX_FRAME_SIZE_BYTES, MAX_HELLO_SIZE_BYTES};

// ---------------------------------------------------------------------------
// Representative sample values. Every field is populated with a small,
// deterministic value (including the u64 capability bitmaps and bytes
// fields). Payload protocols use the frozen registry values: 0x00
// control (Hello/HelloReply), 0xAD01 admin, 0xB232 probe, 0xD0FF handoff.
// ---------------------------------------------------------------------------

fn sample_frame() -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Response as i32,
        payload_protocol: 0xAD01,
        payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
        request_id: 42,
        payload_encoding: PayloadEncoding::Zstd as i32,
        deadline_unix_ms: 1_700_000_000_000,
        traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
        tracestate: "rp=1".into(),
    }
}

fn sample_hello() -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 3,
        service_name: "build-cache".into(),
        wanted_version: "1.2.3-rc.1".into(),
        client_version: "3.0.15".into(),
        // High bit + low bit set: exercises full u64 bitmap width.
        client_capabilities: 0x8000_0000_0000_0001,
        auth_token: vec![0x01, 0x02, 0x03],
        request_id: "req-0001".into(),
        connection_id: 0,
        peer_pid: 4242,
        client_lib_name: "running-process".into(),
        client_lib_version: "3.0.15".into(),
        peer_attestation_nonce: vec![0xAA, 0xBB],
        capability_token: vec![0xCC],
        client_keepalive_secs: 30,
    }
}

fn sample_negotiated() -> Negotiated {
    Negotiated {
        negotiated_protocol: 2,
        daemon_version: "3.0.15".into(),
        backend_pipe: "rp-backend-7".into(),
        warnings: vec!["w1".into(), "w2".into()],
        server_capabilities: 0x8000_0000_0000_0001,
        keepalive_interval_secs: 60,
        handle_passed_token: vec![0x10, 0x20, 0x30],
        connection_id: 99,
    }
}

fn sample_refused() -> Refused {
    // Exactly one map entry — protobuf maps are not order-deterministic
    // across multiple entries (HashMap iteration), so golden samples
    // must use zero or one entry.
    let mut details = HashMap::new();
    details.insert("floor".to_string(), "2".to_string());
    Refused {
        reason: "version below floor".into(),
        daemon_min_protocol: 2,
        daemon_max_protocol: 3,
        code: ErrorCode::ErrorVersionBlocked as i32,
        details,
        retry_after_ms: 1500,
    }
}

fn sample_hello_reply() -> HelloReply {
    HelloReply {
        result: Some(HelloReplyResult::Negotiated(sample_negotiated())),
    }
}

fn sample_handoff_offer() -> HandoffOffer {
    HandoffOffer {
        handle_value: 0x0000_0000_0000_0BEC,
        token: vec![
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
            0x0E, 0x0F,
        ],
        service_name: "build-cache".into(),
        correlation_id: 7,
    }
}

fn sample_handoff_ack() -> HandoffAck {
    HandoffAck {
        token: vec![
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D,
            0x0E, 0x0F,
        ],
        accepted: true,
        error_detail: "ok".into(),
        correlation_id: 7,
    }
}

// ---------------------------------------------------------------------------
// FROZEN golden bytes. Derived once from the samples above; never
// regenerate these from the encoder. A mismatch is a v1 wire break.
// ---------------------------------------------------------------------------

#[rustfmt::skip]
const GOLDEN_FRAME: &[u8] = &[
    0x08, 0x01, 0x10, 0x01, 0x18, 0x81, 0xDA, 0x02, 0x22, 0x04, 0xDE, 0xAD, 0xBE, 0xEF, 0x28,
    0x2A, 0x30, 0x01, 0x38, 0x80, 0xD0, 0x95, 0xFF, 0xBC, 0x31, 0x42, 0x37, 0x30, 0x30, 0x2D,
    0x30, 0x61, 0x66, 0x37, 0x36, 0x35, 0x31, 0x39, 0x31, 0x36, 0x63, 0x64, 0x34, 0x33, 0x64,
    0x64, 0x38, 0x34, 0x34, 0x38, 0x65, 0x62, 0x32, 0x31, 0x31, 0x63, 0x38, 0x30, 0x33, 0x31,
    0x39, 0x63, 0x2D, 0x62, 0x37, 0x61, 0x64, 0x36, 0x62, 0x37, 0x31, 0x36, 0x39, 0x32, 0x30,
    0x33, 0x33, 0x33, 0x31, 0x2D, 0x30, 0x31, 0x4A, 0x04, 0x72, 0x70, 0x3D, 0x31,
];

#[rustfmt::skip]
const GOLDEN_HELLO: &[u8] = &[
    0x08, 0x01, 0x10, 0x03, 0x1A, 0x0B, 0x62, 0x75, 0x69, 0x6C, 0x64, 0x2D, 0x63, 0x61, 0x63,
    0x68, 0x65, 0x22, 0x0A, 0x31, 0x2E, 0x32, 0x2E, 0x33, 0x2D, 0x72, 0x63, 0x2E, 0x31, 0x2A,
    0x06, 0x33, 0x2E, 0x30, 0x2E, 0x31, 0x35, 0x30, 0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80,
    0x80, 0x80, 0x01, 0x3A, 0x03, 0x01, 0x02, 0x03, 0x42, 0x08, 0x72, 0x65, 0x71, 0x2D, 0x30,
    0x30, 0x30, 0x31, 0x50, 0x92, 0x21, 0x5A, 0x0F, 0x72, 0x75, 0x6E, 0x6E, 0x69, 0x6E, 0x67,
    0x2D, 0x70, 0x72, 0x6F, 0x63, 0x65, 0x73, 0x73, 0x62, 0x06, 0x33, 0x2E, 0x30, 0x2E, 0x31,
    0x35, 0x6A, 0x02, 0xAA, 0xBB, 0x72, 0x01, 0xCC, 0x78, 0x1E,
];

#[rustfmt::skip]
const GOLDEN_NEGOTIATED: &[u8] = &[
    0x08, 0x02, 0x12, 0x06, 0x33, 0x2E, 0x30, 0x2E, 0x31, 0x35, 0x1A, 0x0C, 0x72, 0x70, 0x2D,
    0x62, 0x61, 0x63, 0x6B, 0x65, 0x6E, 0x64, 0x2D, 0x37, 0x22, 0x02, 0x77, 0x31, 0x22, 0x02,
    0x77, 0x32, 0x28, 0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01, 0x30, 0x3C,
    0x3A, 0x03, 0x10, 0x20, 0x30, 0x40, 0x63,
];

#[rustfmt::skip]
const GOLDEN_REFUSED: &[u8] = &[
    0x0A, 0x13, 0x76, 0x65, 0x72, 0x73, 0x69, 0x6F, 0x6E, 0x20, 0x62, 0x65, 0x6C, 0x6F, 0x77,
    0x20, 0x66, 0x6C, 0x6F, 0x6F, 0x72, 0x10, 0x02, 0x18, 0x03, 0x20, 0x08, 0x2A, 0x0A, 0x0A,
    0x05, 0x66, 0x6C, 0x6F, 0x6F, 0x72, 0x12, 0x01, 0x32, 0x30, 0xDC, 0x0B,
];

#[rustfmt::skip]
const GOLDEN_HELLO_REPLY: &[u8] = &[
    0x0A, 0x34, 0x08, 0x02, 0x12, 0x06, 0x33, 0x2E, 0x30, 0x2E, 0x31, 0x35, 0x1A, 0x0C, 0x72,
    0x70, 0x2D, 0x62, 0x61, 0x63, 0x6B, 0x65, 0x6E, 0x64, 0x2D, 0x37, 0x22, 0x02, 0x77, 0x31,
    0x22, 0x02, 0x77, 0x32, 0x28, 0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01,
    0x30, 0x3C, 0x3A, 0x03, 0x10, 0x20, 0x30, 0x40, 0x63,
];

#[rustfmt::skip]
const GOLDEN_HANDOFF_OFFER: &[u8] = &[
    0x08, 0xEC, 0x17, 0x12, 0x10, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
    0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F, 0x1A, 0x0B, 0x62, 0x75, 0x69, 0x6C, 0x64, 0x2D, 0x63,
    0x61, 0x63, 0x68, 0x65, 0x20, 0x07,
];

#[rustfmt::skip]
const GOLDEN_HANDOFF_ACK: &[u8] = &[
    0x0A, 0x10, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C,
    0x0D, 0x0E, 0x0F, 0x10, 0x01, 0x1A, 0x02, 0x6F, 0x6B, 0x20, 0x07,
];

fn encode<M: Message>(msg: &M) -> Vec<u8> {
    let mut buf = Vec::new();
    msg.encode(&mut buf).expect("encode into Vec cannot fail");
    buf
}

// ---------------------------------------------------------------------------
// 1) Encode stability: the sample values must serialize to EXACTLY the
//    frozen bytes. A failure here is a v1 wire-format break.
// ---------------------------------------------------------------------------

#[test]
fn frame_encodes_to_golden_bytes() {
    assert_eq!(encode(&sample_frame()), GOLDEN_FRAME);
}

#[test]
fn hello_encodes_to_golden_bytes() {
    assert_eq!(encode(&sample_hello()), GOLDEN_HELLO);
}

#[test]
fn negotiated_encodes_to_golden_bytes() {
    assert_eq!(encode(&sample_negotiated()), GOLDEN_NEGOTIATED);
}

#[test]
fn refused_encodes_to_golden_bytes() {
    assert_eq!(encode(&sample_refused()), GOLDEN_REFUSED);
}

#[test]
fn hello_reply_encodes_to_golden_bytes() {
    assert_eq!(encode(&sample_hello_reply()), GOLDEN_HELLO_REPLY);
}

#[test]
fn handoff_offer_encodes_to_golden_bytes() {
    assert_eq!(encode(&sample_handoff_offer()), GOLDEN_HANDOFF_OFFER);
}

#[test]
fn handoff_ack_encodes_to_golden_bytes() {
    assert_eq!(encode(&sample_handoff_ack()), GOLDEN_HANDOFF_ACK);
}

// ---------------------------------------------------------------------------
// 2) Decode stability: the frozen bytes must decode back into the
//    sample values, field by field. This proves a future prost can
//    still read bytes written by today's prost.
// ---------------------------------------------------------------------------

#[test]
fn golden_frame_decodes_to_sample() {
    let frame = Frame::decode(GOLDEN_FRAME).expect("decode golden Frame");
    let expected = sample_frame();
    assert_eq!(frame.envelope_version, expected.envelope_version);
    assert_eq!(frame.kind, expected.kind);
    assert_eq!(frame.payload_protocol, expected.payload_protocol);
    assert_eq!(frame.payload, expected.payload);
    assert_eq!(frame.request_id, expected.request_id);
    assert_eq!(frame.payload_encoding, expected.payload_encoding);
    assert_eq!(frame.deadline_unix_ms, expected.deadline_unix_ms);
    assert_eq!(frame.traceparent, expected.traceparent);
    assert_eq!(frame.tracestate, expected.tracestate);
}

#[test]
fn golden_hello_decodes_to_sample() {
    let hello = Hello::decode(GOLDEN_HELLO).expect("decode golden Hello");
    let expected = sample_hello();
    assert_eq!(hello.client_min_protocol, expected.client_min_protocol);
    assert_eq!(hello.client_max_protocol, expected.client_max_protocol);
    assert_eq!(hello.service_name, expected.service_name);
    assert_eq!(hello.wanted_version, expected.wanted_version);
    assert_eq!(hello.client_version, expected.client_version);
    assert_eq!(hello.client_capabilities, expected.client_capabilities);
    assert_eq!(hello.auth_token, expected.auth_token);
    assert_eq!(hello.request_id, expected.request_id);
    assert_eq!(hello.connection_id, expected.connection_id);
    assert_eq!(hello.peer_pid, expected.peer_pid);
    assert_eq!(hello.client_lib_name, expected.client_lib_name);
    assert_eq!(hello.client_lib_version, expected.client_lib_version);
    assert_eq!(
        hello.peer_attestation_nonce,
        expected.peer_attestation_nonce
    );
    assert_eq!(hello.capability_token, expected.capability_token);
    assert_eq!(hello.client_keepalive_secs, expected.client_keepalive_secs);
}

#[test]
fn golden_negotiated_decodes_to_sample() {
    let neg = Negotiated::decode(GOLDEN_NEGOTIATED).expect("decode golden Negotiated");
    let expected = sample_negotiated();
    assert_eq!(neg.negotiated_protocol, expected.negotiated_protocol);
    assert_eq!(neg.daemon_version, expected.daemon_version);
    assert_eq!(neg.backend_pipe, expected.backend_pipe);
    assert_eq!(neg.warnings, expected.warnings);
    assert_eq!(neg.server_capabilities, expected.server_capabilities);
    assert_eq!(
        neg.keepalive_interval_secs,
        expected.keepalive_interval_secs
    );
    assert_eq!(neg.handle_passed_token, expected.handle_passed_token);
    assert_eq!(neg.connection_id, expected.connection_id);
}

#[test]
fn golden_refused_decodes_to_sample() {
    let refused = Refused::decode(GOLDEN_REFUSED).expect("decode golden Refused");
    let expected = sample_refused();
    assert_eq!(refused.reason, expected.reason);
    assert_eq!(refused.daemon_min_protocol, expected.daemon_min_protocol);
    assert_eq!(refused.daemon_max_protocol, expected.daemon_max_protocol);
    assert_eq!(refused.code, expected.code);
    assert_eq!(refused.code, ErrorCode::ErrorVersionBlocked as i32);
    assert_eq!(refused.details, expected.details);
    assert_eq!(refused.retry_after_ms, expected.retry_after_ms);
}

#[test]
fn golden_hello_reply_decodes_to_sample() {
    let reply = HelloReply::decode(GOLDEN_HELLO_REPLY).expect("decode golden HelloReply");
    match reply.result {
        Some(HelloReplyResult::Negotiated(neg)) => {
            assert_eq!(neg, sample_negotiated());
        }
        other => panic!("expected Negotiated oneof arm, got {other:?}"),
    }
}

#[test]
fn golden_handoff_offer_decodes_to_sample() {
    let offer = HandoffOffer::decode(GOLDEN_HANDOFF_OFFER).expect("decode golden HandoffOffer");
    let expected = sample_handoff_offer();
    assert_eq!(offer.handle_value, expected.handle_value);
    assert_eq!(offer.token, expected.token);
    assert_eq!(offer.service_name, expected.service_name);
    assert_eq!(offer.correlation_id, expected.correlation_id);
}

#[test]
fn golden_handoff_ack_decodes_to_sample() {
    let ack = HandoffAck::decode(GOLDEN_HANDOFF_ACK).expect("decode golden HandoffAck");
    let expected = sample_handoff_ack();
    assert_eq!(ack.token, expected.token);
    assert_eq!(ack.accepted, expected.accepted);
    assert_eq!(ack.error_detail, expected.error_detail);
    assert_eq!(ack.correlation_id, expected.correlation_id);
}

/// Unknown-field tolerance: a v1 decoder must skip fields it does not
/// know about (proto3 forward compatibility). Append a valid encoding
/// of a hypothetical future field — field number 21 (the first number
/// past Hello's `reserved 16 to 20` range), varint wire type:
/// tag = (21 << 3) | 0 = 0xA8 0x01, value = 1 — and assert decode
/// still succeeds with all known fields intact.
#[test]
fn hello_decode_tolerates_unknown_trailing_field() {
    let mut extended = GOLDEN_HELLO.to_vec();
    extended.extend_from_slice(&[0xA8, 0x01, 0x01]);
    let hello = Hello::decode(extended.as_slice()).expect("decode Hello with unknown field");
    assert_eq!(hello, sample_hello(), "known fields must be intact");
}

// ---------------------------------------------------------------------------
// 3) Framing layout: `[u8 version=1][u32 LE body_length][body]` plus
//    the frozen size caps and oversize FramingError mapping.
// ---------------------------------------------------------------------------

#[test]
fn framing_layout_is_version_byte_then_le_length_then_body() {
    let body = encode(&sample_hello());
    let mut wire = Vec::new();
    let written = write_frame(&mut wire, &body).expect("write frame");
    assert_eq!(written, 5 + body.len());

    // Byte 0: framing version, frozen at 1.
    assert_eq!(wire[0], 1, "framing version byte must be 1 forever in v1");
    assert_eq!(ENVELOPE_VERSION, FRAMING_VERSION_V1);
    assert_eq!(FRAMING_VERSION_V1, 1);

    // Bytes 1..5: body length as little-endian u32.
    assert_eq!(&wire[1..5], (body.len() as u32).to_le_bytes());

    // Bytes 5..: the prost body, verbatim.
    assert_eq!(&wire[5..], body.as_slice());

    // And the whole frame reads back.
    let mut cursor = Cursor::new(wire);
    assert_eq!(read_frame(&mut cursor).expect("read frame"), body);
}

#[test]
fn framed_golden_hello_has_exact_header_bytes() {
    // GOLDEN_HELLO is 100 bytes, so the full frozen header is
    // [0x01, 0x64, 0x00, 0x00, 0x00].
    let mut wire = Vec::new();
    write_frame(&mut wire, GOLDEN_HELLO).expect("write frame");
    assert_eq!(GOLDEN_HELLO.len(), 100);
    assert_eq!(&wire[..5], &[0x01, 0x64, 0x00, 0x00, 0x00]);
    assert_eq!(&wire[5..], GOLDEN_HELLO);
}

#[test]
fn size_cap_constants_are_frozen() {
    assert_eq!(MAX_FRAME_SIZE_BYTES, 16 * 1024 * 1024, "16 MiB frame cap");
    assert_eq!(MAX_HELLO_SIZE_BYTES, 64 * 1024, "64 KiB Hello cap");
}

#[test]
fn oversize_frame_maps_to_frame_too_large() {
    // A header claiming MAX_FRAME_SIZE_BYTES + 1 must be rejected
    // before any body read, with the FrameTooLarge mapping.
    let mut wire = Vec::new();
    wire.push(FRAMING_VERSION_V1);
    wire.extend_from_slice(&((MAX_FRAME_SIZE_BYTES as u32) + 1).to_le_bytes());
    let mut cursor = Cursor::new(wire);
    match read_frame(&mut cursor).unwrap_err() {
        FramingError::FrameTooLarge { body_length, cap } => {
            assert_eq!(body_length, MAX_FRAME_SIZE_BYTES + 1);
            assert_eq!(cap, MAX_FRAME_SIZE_BYTES);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}

#[test]
fn oversize_hello_maps_to_frame_too_large_under_hello_cap() {
    let mut wire = Vec::new();
    wire.push(FRAMING_VERSION_V1);
    wire.extend_from_slice(&((MAX_HELLO_SIZE_BYTES as u32) + 1).to_le_bytes());
    let mut cursor = Cursor::new(wire);
    match read_frame_with_cap(&mut cursor, MAX_HELLO_SIZE_BYTES).unwrap_err() {
        FramingError::FrameTooLarge { body_length, cap } => {
            assert_eq!(body_length, MAX_HELLO_SIZE_BYTES + 1);
            assert_eq!(cap, MAX_HELLO_SIZE_BYTES);
        }
        other => panic!("expected FrameTooLarge, got {other:?}"),
    }
}
