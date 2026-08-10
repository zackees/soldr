//! Phase 0 of #228 — every LifecycleEvent encoded with any EventKind
//! value (including reserved-range placeholders) MUST fit in the
//! cross-platform POSIX PIPE_BUF floor of 512 bytes so atomic-append
//! into the event log is guaranteed on every platform.

#![cfg(feature = "client")]

use std::collections::HashMap;

use prost::Message;
use running_process::broker::protocol::{EventKind, LifecycleEvent};
use running_process::broker::LIFECYCLE_EVENT_PIPE_BUF_FLOOR;

fn worst_case_event(kind: i32) -> LifecycleEvent {
    // Pack realistic-but-bounded values into every field. The broker
    // doesn't accept unbounded strings on the wire; this matches the
    // documented field caps in #228.
    let mut extra = HashMap::new();
    extra.insert("k1".into(), "v1".into());
    extra.insert("k2".into(), "v2".into());
    LifecycleEvent {
        ts_ms: u64::MAX,
        pid: u32::MAX,
        service_name: "a".repeat(64),
        kind,
        reason: "x".repeat(80),
        extra,
        severity_number: 24,
        severity_text: "CRITICAL".into(),
        request_id: "r".repeat(32),
        connection_id: u64::MAX,
        broker_instance: "instance-name-padded".into(),
    }
}

fn enum_values_to_probe() -> Vec<i32> {
    // Every documented variant plus a sampling of reserved-range values
    // (21..30, 31..40, 41..50). Reserved values won't decode as a
    // named EventKind but the wire encoding is fully defined, so the
    // PIPE_BUF gate must hold for them too.
    let mut values: Vec<i32> = vec![
        EventKind::Unspecified as i32,
        EventKind::SpawnAttempt as i32,
        EventKind::Spawn as i32,
        EventKind::DiedIdle as i32,
        EventKind::DiedShutdown as i32,
        EventKind::DiedCrash as i32,
        EventKind::VersionMismatch as i32,
        EventKind::ReplacedByNewer as i32,
        EventKind::HelloAccepted as i32,
        EventKind::HelloRefused as i32,
        EventKind::ServiceDefLoaded as i32,
        EventKind::ServiceDefChanged as i32,
        EventKind::BroadcastSent as i32,
        EventKind::BroadcastAck as i32,
        EventKind::BroadcastTimeout as i32,
        EventKind::ProtocolDowngrade as i32,
        EventKind::CacheCorruptionDetected as i32,
        EventKind::ResourcePressure as i32,
        EventKind::SecurityViolation as i32,
        EventKind::TeardownHookFailed as i32,
        EventKind::ManifestRewritten as i32,
    ];
    values.extend(21..=50);
    values
}

#[test]
fn worst_case_lifecycle_event_fits_in_pipe_buf_floor() {
    for kind in enum_values_to_probe() {
        let evt = worst_case_event(kind);
        let mut buf = Vec::new();
        evt.encode(&mut buf).expect("encode");
        assert!(
            buf.len() <= LIFECYCLE_EVENT_PIPE_BUF_FLOOR,
            "EventKind={} encoded to {} bytes; PIPE_BUF floor is {}",
            kind,
            buf.len(),
            LIFECYCLE_EVENT_PIPE_BUF_FLOOR,
        );
    }
}
