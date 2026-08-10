#![no_main]

use std::sync::LazyLock;
use std::time::Duration;

use libfuzzer_sys::fuzz_target;
use prost::Message;
use running_process::broker::protocol::{
    AdminReply, AdminRequest, Frame, FrameKind, PayloadEncoding, PROTOCOL_VERSION,
};
use running_process::broker::server::admin::{
    handle_admin_frame, AdminInodePressure, AdminSnapshot, ADMIN_PAYLOAD_PROTOCOL,
};

mod common;

/// Deterministic snapshot so dispatch never touches the live filesystem.
static SNAPSHOT: LazyLock<AdminSnapshot> = LazyLock::new(|| AdminSnapshot {
    broker_instance: "fuzz".into(),
    broker_pid: 4242,
    generated_at_unix_ms: 0,
    uptime: Duration::ZERO,
    accepting_hello: true,
    connections_open: 0,
    backends: Vec::new(),
    spawn_budgets: Vec::new(),
    fd_pressure_demoted: false,
    inode_pressure: AdminInodePressure::default(),
});

const FUZZ_REQUEST_ID: u64 = 7;

fuzz_target!(|data: &[u8]| {
    if common::skip_oversize_proto_input(data) {
        return;
    }

    // Client-side reply decode surface.
    let _ = AdminReply::decode(data);

    // Untrusted admin frame straight off the wire: envelope validation plus
    // AdminRequest payload decode.
    if let Ok(frame) = Frame::decode(data) {
        let _ = handle_admin_frame(frame, &SNAPSHOT);
    }

    // Wrap arbitrary bytes in a well-formed envelope so the fuzzer reaches
    // the AdminRequest decode and verb dispatch directly.
    let frame = Frame {
        envelope_version: PROTOCOL_VERSION,
        kind: FrameKind::Request as i32,
        payload_protocol: ADMIN_PAYLOAD_PROTOCOL,
        payload: data.to_vec(),
        request_id: FUZZ_REQUEST_ID,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    let decodes = AdminRequest::decode(data).is_ok();
    match handle_admin_frame(frame, &SNAPSHOT) {
        Ok(response) => {
            assert!(
                decodes,
                "handle_admin_frame accepted a payload AdminRequest::decode rejects"
            );
            assert_eq!(
                response.request_id, FUZZ_REQUEST_ID,
                "admin response must echo the request id"
            );
            AdminReply::decode(response.payload.as_slice())
                .expect("admin response payload must decode as AdminReply");
        }
        Err(_) => {
            assert!(
                !decodes,
                "handle_admin_frame rejected a valid envelope with a decodable AdminRequest"
            );
        }
    }
});
