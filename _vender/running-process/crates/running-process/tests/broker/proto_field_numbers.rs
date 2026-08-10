//! Phase 0 of #228 — verify the proto sources keep every `reserved`
//! range that the v1 contract calls out. prost doesn't surface
//! `reserved` ranges in its generated Rust types, so the cheapest
//! authoritative check is to grep the on-disk proto files for the
//! exact tokens. If anyone ever deletes a reserve to free a slot, this
//! test fails and the buf-breaking CI gate has a backup smoke alarm.

use std::path::PathBuf;

fn proto_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("proto")
        .join("broker_v1")
}

fn proto(name: &str) -> String {
    let path = proto_root().join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn envelope_reserved_ranges_present() {
    let src = proto("broker_v1_envelope.proto");
    // Frame.reserved 10 to 15 (hot-path 1-byte-tag slots)
    assert!(
        src.contains("reserved 10 to 15"),
        "Frame must reserve field numbers 10..15"
    );
    // Hello.reserved 16 to 20
    assert!(
        src.contains("reserved 16 to 20"),
        "Hello must reserve field numbers 16..20"
    );
    // ErrorCode.reserved 10 to 20 (variant numbers, but same token)
    assert!(
        src.contains("reserved 10 to 20"),
        "ErrorCode must reserve enum values 10..20"
    );
    // HandoffOffer and HandoffAck each reserve 5..10 (#354 slice 6)
    assert!(
        src.matches("reserved 5 to 10").count() >= 2,
        "HandoffOffer and HandoffAck must each reserve field numbers 5..10"
    );
}

#[test]
fn manifest_reserved_ranges_present() {
    let src = proto("broker_v1_manifest.proto");
    // CacheManifest reserves 110..199 and 200..255
    assert!(
        src.contains("reserved 110 to 199"),
        "CacheManifest must reserve 110..199 for v1.x cleanup-policy expansions"
    );
    assert!(
        src.contains("reserved 200 to 255"),
        "CacheManifest must reserve 200..255 for long-range future"
    );
    // CacheRootKind reserves 10..20
    assert!(
        src.contains("reserved 10 to 20"),
        "CacheRootKind must reserve enum values 10..20"
    );
    // StorageDisposition reserves 5..10
    assert!(
        src.contains("reserved 5 to 10"),
        "StorageDisposition must reserve enum values 5..10"
    );
    // EventKind reserves 21..30, 31..40, 41..50
    assert!(
        src.contains("reserved 21 to 30"),
        "EventKind must reserve 21..30 for v1.x resource-pressure events"
    );
    assert!(
        src.contains("reserved 31 to 40"),
        "EventKind must reserve 31..40 for v1.x cleanup/doctor events"
    );
    assert!(
        src.contains("reserved 41 to 50"),
        "EventKind must reserve 41..50 for long-range future"
    );
}

#[test]
fn admin_reserved_ranges_present() {
    let src = proto("broker_v1_admin.proto");
    assert!(
        src.matches("reserved 10 to 20").count() >= 4,
        "AdminRequest, AdminReply, AdminVerb, and AdminReplyKind must reserve 10..20"
    );
}

#[test]
fn frozen_package_and_syntax() {
    for name in &[
        "broker_v1_envelope.proto",
        "broker_v1_admin.proto",
        "broker_v1_manifest.proto",
        "broker_v1_service_def.proto",
    ] {
        let src = proto(name);
        assert!(
            src.contains("syntax = \"proto3\""),
            "{name}: must declare proto3 syntax"
        );
        assert!(
            src.contains("package running_process.broker.v1;"),
            "{name}: must declare the frozen v1 package"
        );
    }
}

#[test]
fn envelope_framing_byte_documented() {
    // The framing-byte invariant is text-only commentary in the proto
    // file; a regression here means somebody deleted the wire-level
    // commitments comment block.
    let src = proto("broker_v1_envelope.proto");
    assert!(
        src.contains("framing_version=1"),
        "envelope must document framing_version=1 in wire-level commitments"
    );
    assert!(
        src.contains("Max frame size: 16 MiB"),
        "envelope must document 16 MiB max frame size"
    );
    assert!(
        src.contains("Max Hello size: 64 KiB"),
        "envelope must document 64 KiB max Hello size"
    );
}
