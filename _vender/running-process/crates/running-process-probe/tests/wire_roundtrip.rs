//! Wire contract for `running_process.probe_diag.v1` (#630).
//!
//! These tests freeze the schema's observable behavior: that every top-level
//! message survives a frame round-trip, that a committed golden encoding still
//! decodes, that unknown fields are tolerated, and that the shared 16 MiB frame
//! cap is enforced rather than re-derived.
//!
//! The golden fixtures are the load-bearing part. A round-trip test only proves
//! encode and decode agree *with each other* — it passes even if a field number
//! changes, because both sides changed together. The committed `.bin` files are
//! bytes from before any such edit, so they catch a renumbering that a
//! round-trip cannot. Regenerate them only when intentionally breaking the wire
//! (see `regenerate_golden_fixtures` below).

use prost::Message;
use running_process::broker::protocol::framing::{
    read_frame, write_frame, FramingError, MAX_FRAME_BYTES,
};
use running_process_probe::probe_diag::v1::{
    probe_envelope::Body, CaptureStackRequest, CrashRecord, ProbeEnvelope, ProcessKey,
    RegisterProcess, StartProfile,
};

/// Wrap a body in an envelope with fixed header values.
///
/// Constants, not clock reads — a fixture generated from `now()` would encode
/// differently on every run and could never be committed.
fn envelope(request_id: u64, body: Body) -> ProbeEnvelope {
    ProbeEnvelope {
        wire_version: 1,
        request_id,
        deadline_unix_ms: 1_700_000_000_000,
        body: Some(body),
    }
}

fn sample_key() -> ProcessKey {
    ProcessKey {
        pid: 4242,
        start_time: Some(999),
        boot_id: Some("boot-xyz".into()),
    }
}

fn sample_register() -> ProbeEnvelope {
    envelope(
        7,
        Body::Register(RegisterProcess {
            key: Some(sample_key()),
            exe_path: "/usr/bin/app".into(),
            app_class: "clud".into(),
            ..Default::default()
        }),
    )
}

fn sample_capture() -> ProbeEnvelope {
    envelope(
        8,
        Body::CaptureStack(CaptureStackRequest {
            key: Some(sample_key()),
            max_depth: 128,
            thread_filter: 0,
            ..Default::default()
        }),
    )
}

fn sample_profile() -> ProbeEnvelope {
    envelope(
        9,
        Body::StartProfile(StartProfile {
            key: Some(sample_key()),
            kind: 1,
            hz: 99,
            duration_ms: 60_000,
        }),
    )
}

fn sample_crash() -> ProbeEnvelope {
    envelope(
        10,
        Body::CrashRecord(CrashRecord {
            key: Some(sample_key()),
            signature: "SIGSEGV@main+0x40".into(),
            crash_unix_ms: 1_700_000_000_123,
            // An opaque id, not a path: the crash query surface returns
            // redacted metadata, and a daemon-private path would disclose the
            // owner's directory layout to every caller of it.
            id: 42,
            artifact_bytes: 4096,
            fault_kind: "SIGSEGV".into(),
            ..Default::default()
        }),
    )
}

/// Encode, frame, unframe, decode — assert structural equality.
fn assert_frame_roundtrip(msg: &ProbeEnvelope) {
    let body = msg.encode_to_vec();

    let mut wire = Vec::new();
    write_frame(&mut wire, &body).expect("write_frame");

    let read = read_frame(&mut wire.as_slice()).expect("read_frame");
    let got = ProbeEnvelope::decode(read.as_slice()).expect("decode");

    assert_eq!(&got, msg, "envelope did not survive the frame round-trip");
}

#[test]
fn register_frame_roundtrips() {
    assert_frame_roundtrip(&sample_register());
}

#[test]
fn capture_frame_roundtrips() {
    assert_frame_roundtrip(&sample_capture());
}

#[test]
fn profile_frame_roundtrips() {
    assert_frame_roundtrip(&sample_profile());
}

#[test]
fn crash_frame_roundtrips() {
    assert_frame_roundtrip(&sample_crash());
}

/// Golden fixtures — the actual wire freeze. See the module docs.
#[test]
fn golden_fixtures_decode_stable() {
    let cases: &[(&[u8], ProbeEnvelope)] = &[
        (
            include_bytes!("fixtures/probe_diag_v1_register.bin"),
            sample_register(),
        ),
        (
            include_bytes!("fixtures/probe_diag_v1_capture.bin"),
            sample_capture(),
        ),
        (
            include_bytes!("fixtures/probe_diag_v1_profile.bin"),
            sample_profile(),
        ),
        (
            include_bytes!("fixtures/probe_diag_v1_crash.bin"),
            sample_crash(),
        ),
    ];

    for (bytes, expected) in cases {
        let got = ProbeEnvelope::decode(*bytes).expect("golden fixture failed to decode");
        assert_eq!(
            &got, expected,
            "golden fixture decoded to a different value — a field number or \
             type changed, which breaks compatibility with already-encoded data"
        );
    }
}

/// A peer on a newer schema may send fields this build has never heard of.
/// Those must be skipped, leaving the known fields intact — otherwise every
/// additive schema change would be a breaking one.
#[test]
fn unknown_field_is_ignored() {
    let mut body = sample_register().encode_to_vec();

    // Field 900, wire-type 0 (varint): tag = (900 << 3) | 0 = 7200,
    // encoded as the varint 0xA0 0x38, followed by the value 42.
    body.extend_from_slice(&[0xA0, 0x38, 0x2A]);

    let got = ProbeEnvelope::decode(body.as_slice())
        .expect("unknown field must be skipped, not rejected");
    assert_eq!(got, sample_register());
}

/// An unknown enum value must not fail the decode. proto3 keeps it as its raw
/// i32 so a newer peer's variant degrades to "unrecognized" instead of
/// dropping the whole message.
#[test]
fn unknown_enum_value_is_preserved_as_raw_i32() {
    let mut msg = sample_profile();
    let Some(Body::StartProfile(ref mut p)) = msg.body else {
        panic!("sample_profile must carry a StartProfile body");
    };
    p.kind = 4242; // not a declared ProfileKind variant

    let encoded = msg.encode_to_vec();
    let got = ProbeEnvelope::decode(encoded.as_slice()).expect("decode with unknown enum");

    let Some(Body::StartProfile(p)) = got.body else {
        panic!("body variant changed across the round-trip");
    };
    assert_eq!(p.kind, 4242, "unknown enum value must survive as raw i32");
}

/// The 16 MiB cap is the broker's, reused. This asserts the probe wire is
/// actually bound by it rather than quietly accepting a larger frame.
#[test]
fn oversize_frame_is_rejected() {
    let mut wire = vec![1u8];
    wire.extend_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes());
    wire.extend_from_slice(&[0u8; 64]); // header claims more than it carries

    assert!(
        matches!(
            read_frame(&mut wire.as_slice()),
            Err(FramingError::FrameTooLarge { .. })
        ),
        "a frame claiming more than MAX_FRAME_BYTES must be refused on the header"
    );
}

/// Decoding attacker-controlled bytes must never panic — it may only return
/// `Err`. A panic in a daemon that decodes untrusted input is a DoS.
mod fuzz {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn decode_arbitrary_bytes_never_panics(bytes: Vec<u8>) {
            let _ = ProbeEnvelope::decode(bytes.as_slice());
        }

        #[test]
        fn decode_corrupted_valid_encoding_never_panics(
            idx in 0usize..64,
            xor in 1u8..=255,
        ) {
            let mut body = sample_register().encode_to_vec();
            if idx < body.len() {
                body[idx] ^= xor;
            }
            let _ = ProbeEnvelope::decode(body.as_slice());
        }
    }
}

/// Writer for the golden fixtures. Ignored so it never runs in CI — the point
/// of a frozen fixture is that CI compares against it rather than rewriting it.
///
/// Run deliberately, and only when intentionally breaking the wire:
///   soldr cargo test -p running-process-probe --test wire_roundtrip \
///       -- --ignored regenerate_golden_fixtures
#[test]
#[ignore = "regenerates committed wire fixtures; run only on a deliberate wire change"]
fn regenerate_golden_fixtures() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    std::fs::create_dir_all(&dir).expect("create fixtures dir");

    for (name, msg) in [
        ("probe_diag_v1_register.bin", sample_register()),
        ("probe_diag_v1_capture.bin", sample_capture()),
        ("probe_diag_v1_profile.bin", sample_profile()),
        ("probe_diag_v1_crash.bin", sample_crash()),
    ] {
        std::fs::write(dir.join(name), msg.encode_to_vec()).expect("write fixture");
    }
}
