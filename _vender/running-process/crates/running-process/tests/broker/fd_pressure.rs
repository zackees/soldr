#![cfg(feature = "client")]
//! FD-pressure self-demotion integration tests (#390).
//!
//! Real fd exhaustion is hard to stage portably, so these tests drive
//! the demotion state machine with injected accept errors (which is the
//! exact seam the production accept loop uses) and additionally — on
//! Unix only — lower `RLIMIT_NOFILE` to produce a genuine EMFILE and
//! assert the classifier recognizes it.

use std::io::{self, Read, Write};

use prost::Message;
use running_process::broker::protocol::{
    hello_reply::Result as HelloReplyResult, read_frame, AdminReply, AdminReplyKind, AdminRequest,
    AdminVerb, BrokerIsolation, ErrorCode, Frame, FrameKind, Hello, HelloReply, PayloadEncoding,
    Refused, ServiceDefinition,
};
use running_process::broker::server::{
    fd_exhaustion_error_for_tests, handle_control_connection_with_peer_policy_and_fd_guard,
    is_fd_exhaustion_error, AdminSnapshot, ControlSocketReply, FdPressureDecision, FdPressureGuard,
    HelloHandler, PeerCredentialPolicy, PeerIdentity, RegisteredBackend, ADMIN_PAYLOAD_PROTOCOL,
    DEFAULT_FD_PRESSURE_RECOVERY_ACCEPTS, DEFAULT_FD_PRESSURE_RETRY_AFTER_MS,
};

fn service_definition() -> ServiceDefinition {
    ServiceDefinition {
        service_name: "zccache".into(),
        binary_path: "/usr/local/bin/zccache".into(),
        isolation: BrokerIsolation::SharedBroker as i32,
        explicit_instance: String::new(),
        per_version_binary_dir: "/opt/zccache/versions".into(),
        min_version: "1.10.0".into(),
        version_allow_list: vec!["1.11.20".into()],
        labels: Default::default(),
    }
}

fn handler() -> HelloHandler {
    HelloHandler::new()
        .with_backend(RegisteredBackend {
            service_definition: service_definition(),
            daemon_version: "1.11.20".into(),
            backend_pipe: r"\\.\pipe\rpb-v1-fd-pressure-test-backend".into(),
            server_capabilities: 0x01,
        })
        .unwrap()
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
        request_id: "req-fd-pressure".into(),
        connection_id: 0,
        peer_pid: std::process::id(),
        client_lib_name: "running-process".into(),
        client_lib_version: "4.0.3".into(),
        peer_attestation_nonce: Vec::new(),
        capability_token: Vec::new(),
        client_keepalive_secs: 60,
    }
}

fn frame_for_hello(request: &Hello) -> Frame {
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: request.encode_to_vec(),
        request_id: 41,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn status_json_frame() -> Frame {
    let request = AdminRequest {
        verb: AdminVerb::Status as i32,
        json: true,
        service_name: String::new(),
        output_path: String::new(),
    };
    Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: ADMIN_PAYLOAD_PROTOCOL,
        payload: request.encode_to_vec(),
        request_id: 42,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    }
}

fn peer() -> PeerIdentity {
    PeerIdentity {
        pid: std::process::id(),
        uid_or_sid: "test-peer".into(),
    }
}

fn refused(reply: HelloReply) -> Refused {
    match reply.result.unwrap() {
        HelloReplyResult::Refused(refused) => refused,
        HelloReplyResult::Negotiated(negotiated) => {
            panic!("expected refusal, got negotiated {negotiated:?}")
        }
    }
}

/// In-memory `Read + Write` stand-in for an accepted control connection:
/// reads from a pre-encoded request, captures the response bytes.
struct MockStream {
    input: io::Cursor<Vec<u8>>,
    output: Vec<u8>,
}

impl MockStream {
    fn with_frame(frame: &Frame) -> Self {
        let mut bytes = Vec::new();
        running_process::broker::protocol::write_frame(&mut bytes, &frame.encode_to_vec()).unwrap();
        Self {
            input: io::Cursor::new(bytes),
            output: Vec::new(),
        }
    }

    fn response_frame(&self) -> Frame {
        let mut reader = self.output.as_slice();
        let bytes = read_frame(&mut reader).unwrap();
        Frame::decode(bytes.as_slice()).unwrap()
    }
}

impl Read for MockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.input.read(buf)
    }
}

impl Write for MockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.output.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn dispatch(stream: &mut MockStream, guard: &FdPressureGuard) -> ControlSocketReply {
    let responder = handler();
    let guard_ref = guard;
    handle_control_connection_with_peer_policy_and_fd_guard(
        stream,
        &responder,
        &move || {
            AdminSnapshot::local_not_serving().with_fd_pressure_demoted(guard_ref.is_demoted())
        },
        peer(),
        &PeerCredentialPolicy::allow_any(),
        Some(guard),
    )
    .unwrap()
}

#[test]
fn demoted_guard_refuses_hello_with_fd_pressure_code_on_the_wire() {
    let guard = FdPressureGuard::default();
    assert_eq!(
        guard.on_accept_error(&fd_exhaustion_error_for_tests()),
        FdPressureDecision::Demoted
    );

    let mut stream = MockStream::with_frame(&frame_for_hello(&hello()));
    let reply = dispatch(&mut stream, &guard);

    let ControlSocketReply::Hello(hello_reply) = reply else {
        panic!("expected Hello dispatch, got {reply:?}");
    };
    let refusal = refused(hello_reply);
    assert_eq!(
        ErrorCode::try_from(refusal.code),
        Ok(ErrorCode::ErrorFdPressure)
    );
    assert_eq!(refusal.retry_after_ms, DEFAULT_FD_PRESSURE_RETRY_AFTER_MS);
    assert_eq!(guard.refused_while_demoted(), 1);

    // The refusal must also be what actually went over the wire.
    let response_frame = stream.response_frame();
    assert_eq!(
        FrameKind::try_from(response_frame.kind),
        Ok(FrameKind::Response)
    );
    assert_eq!(response_frame.request_id, 41);
    let wire_reply = HelloReply::decode(response_frame.payload.as_slice()).unwrap();
    assert_eq!(
        ErrorCode::try_from(refused(wire_reply).code),
        Ok(ErrorCode::ErrorFdPressure)
    );
}

#[test]
fn admin_status_still_served_while_demoted_and_reports_demotion() {
    let guard = FdPressureGuard::default();
    guard.on_accept_error(&fd_exhaustion_error_for_tests());

    let mut stream = MockStream::with_frame(&status_json_frame());
    let reply = dispatch(&mut stream, &guard);

    let ControlSocketReply::Admin(admin_reply) = reply else {
        panic!("expected admin dispatch while demoted, got {reply:?}");
    };
    assert_eq!(admin_reply.exit_code, 0);
    assert_eq!(
        AdminReplyKind::try_from(admin_reply.kind),
        Ok(AdminReplyKind::Json)
    );
    let value: serde_json::Value = serde_json::from_str(&admin_reply.body).unwrap();
    assert_eq!(value["fd_pressure"]["demoted"], true);

    // And the same bytes round-trip on the wire.
    let response_frame = stream.response_frame();
    let wire_reply = AdminReply::decode(response_frame.payload.as_slice()).unwrap();
    assert_eq!(wire_reply.body, admin_reply.body);
}

#[test]
fn hello_serves_normally_after_recovery_streak() {
    let guard = FdPressureGuard::default();
    guard.on_accept_error(&fd_exhaustion_error_for_tests());
    assert!(guard.is_demoted());

    let mut recovered = false;
    for _ in 0..DEFAULT_FD_PRESSURE_RECOVERY_ACCEPTS {
        recovered = guard.on_accept_ok();
    }
    assert!(recovered, "recovery streak should clear the demotion");
    assert!(!guard.is_demoted());

    let mut stream = MockStream::with_frame(&frame_for_hello(&hello()));
    let reply = dispatch(&mut stream, &guard);
    let ControlSocketReply::Hello(hello_reply) = reply else {
        panic!("expected Hello dispatch, got {reply:?}");
    };
    match hello_reply.result.unwrap() {
        HelloReplyResult::Negotiated(negotiated) => {
            assert_eq!(negotiated.daemon_version, "1.11.20");
        }
        HelloReplyResult::Refused(refusal) => panic!("unexpected refusal: {refusal:?}"),
    }
}

/// Windows (and everywhere): the state machine driven purely by injected
/// errors, no real fd exhaustion required.
#[test]
fn injected_errors_drive_demotion_and_unrelated_errors_do_not() {
    let guard = FdPressureGuard::default();

    let unrelated = io::Error::new(io::ErrorKind::ConnectionReset, "peer reset");
    assert_eq!(
        guard.on_accept_error(&unrelated),
        FdPressureDecision::Unrelated
    );
    assert!(!guard.is_demoted());

    assert!(is_fd_exhaustion_error(&fd_exhaustion_error_for_tests()));
    assert_eq!(
        guard.on_accept_error(&fd_exhaustion_error_for_tests()),
        FdPressureDecision::Demoted
    );
    assert!(guard.is_demoted());
    assert_eq!(guard.demotions_total(), 1);
}

/// Unix only: lower `RLIMIT_NOFILE` until `dup(2)` hits real EMFILE and
/// assert the classifier recognizes the genuine kernel error. nextest
/// runs each test in its own process, but the original limit is restored
/// anyway so plain `cargo test` stays safe.
#[cfg(unix)]
#[test]
fn real_emfile_from_lowered_rlimit_is_classified_as_fd_exhaustion() {
    unsafe {
        let mut original = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            libc::getrlimit(libc::RLIMIT_NOFILE, &mut original),
            0,
            "getrlimit failed: {}",
            io::Error::last_os_error()
        );

        // Burn descriptors up to the current soft limit by dup()ing
        // stderr until the kernel refuses with EMFILE.
        let lowered = libc::rlimit {
            rlim_cur: 64,
            rlim_max: original.rlim_max,
        };
        assert_eq!(
            libc::setrlimit(libc::RLIMIT_NOFILE, &lowered),
            0,
            "setrlimit failed: {}",
            io::Error::last_os_error()
        );

        let mut burned: Vec<libc::c_int> = Vec::new();
        let exhaustion_error = loop {
            let fd = libc::dup(2);
            if fd < 0 {
                break io::Error::last_os_error();
            }
            burned.push(fd);
            assert!(
                burned.len() <= 4096,
                "never hit EMFILE despite lowered RLIMIT_NOFILE"
            );
        };

        // Restore the process before asserting so a failure cannot
        // poison later tests under plain `cargo test`.
        for fd in burned {
            libc::close(fd);
        }
        assert_eq!(libc::setrlimit(libc::RLIMIT_NOFILE, &original), 0);

        assert!(
            is_fd_exhaustion_error(&exhaustion_error),
            "expected EMFILE/ENFILE classification, got {exhaustion_error:?}"
        );
        let guard = FdPressureGuard::default();
        assert_eq!(
            guard.on_accept_error(&exhaustion_error),
            FdPressureDecision::Demoted
        );
        assert!(guard.is_demoted());
    }
}
