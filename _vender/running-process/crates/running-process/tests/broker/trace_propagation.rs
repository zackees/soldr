#![cfg(feature = "client")]

use prost::Message;
use running_process::broker::protocol::{Frame, FrameKind, Hello, PayloadEncoding};
use running_process::broker::server::{HelloRequest, PeerIdentity};

fn peer() -> PeerIdentity {
    PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: "test-peer".into(),
    }
}

fn hello() -> Hello {
    Hello {
        client_min_protocol: 1,
        client_max_protocol: 1,
        service_name: "zccache".into(),
        wanted_version: "1.11.20".into(),
        client_version: "zccache-cli/1.11.20".into(),
        client_capabilities: 0,
        auth_token: Vec::new(),
        request_id: "req-trace".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

#[test]
fn hello_request_trace_context_is_backend_forwardable() {
    let traceparent = "00-11111111111111111111111111111111-2222222222222222-01";
    let tracestate = "vendor=value";
    let frame = Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: hello().encode_to_vec(),
        request_id: 42,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: traceparent.into(),
        tracestate: tracestate.into(),
    };

    let request = HelloRequest::decode(frame, peer()).unwrap();
    let context = request.trace_context();

    assert_eq!(context.request_id, 42);
    assert_eq!(context.traceparent, traceparent);
    assert_eq!(context.tracestate, tracestate);
    assert_eq!(
        context.backend_headers(),
        vec![
            ("traceparent", traceparent.to_string()),
            ("tracestate", tracestate.to_string())
        ]
    );
}
