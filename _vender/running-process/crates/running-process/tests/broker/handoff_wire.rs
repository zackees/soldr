//! Wire-frame handoff offer/ACK delivery (#354, slice 6).
//!
//! Platform-neutral coverage for the production `WireHandoffDelivery`
//! path: a broker side sends a `HandoffOffer` frame over an in-process
//! local-socket pair, the backend helper validates/consumes the one-time
//! token via `accept_handed_off` and replies with a `HandoffAck`, and the
//! Windows orchestration state machine (driven with a mock duplication
//! transport so it runs on every platform) completes or falls back.

#![cfg(feature = "client")]

use std::io;
use std::thread;
use std::time::{Duration, Instant};

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{ListenerOptions, Stream};
use prost::Message;
use running_process::broker::backend_lib::{
    read_handoff_offer, serve_handoff_offer_with_deadline, write_handoff_ack,
    BackendHandoffWireError, HandoffAcceptance, HandoffRejectionReason,
};
use running_process::broker::protocol::{
    read_frame, write_frame, Frame, FrameKind, FramingError, HandoffAck, HandoffOffer,
    PayloadEncoding,
};
use running_process::broker::server::handoff::{
    execute_windows_handoff_with_transport, handoff_ack_frame, handoff_offer_frame,
    DuplicateHandleAttempt, DuplicateHandleResult, DuplicateHandleSuccess, HandoffAckRegistry,
    HandoffDelivery, HandoffDeliveryError, HandoffToken, HandoffTokenStore, PendingHandoffBackend,
    WindowsHandleValue, WindowsHandoffOutcome, WindowsHandoffRequest, WindowsHandoffStage,
    WireHandoffDelivery, HANDOFF_PAYLOAD_PROTOCOL,
};
use running_process::broker::server::local_socket_name;

use crate::socket_common::unique_socket_name;

const BACKEND_PID: u32 = 4242;
const SERVICE: &str = "zccache";
const CORRELATION_ID: u64 = 0xC0FFEE;

fn token(byte: u8) -> HandoffToken {
    HandoffToken::from_bytes([byte; 16])
}

fn issue_token(store: &mut HandoffTokenStore, now: Instant, byte: u8) -> HandoffToken {
    store
        .issue_with_random128(now, || Ok(token(byte).into_bytes()))
        .unwrap()
}

fn issue_registered_token(
    tokens: &mut HandoffTokenStore,
    acks: &mut HandoffAckRegistry,
    now: Instant,
    byte: u8,
) -> HandoffToken {
    let issued = issue_token(tokens, now, byte);
    acks.register(
        issued,
        PendingHandoffBackend::new(SERVICE, BACKEND_PID),
        now,
    );
    issued
}

fn request(issued: HandoffToken) -> WindowsHandoffRequest {
    WindowsHandoffRequest::new(WindowsHandleValue::new(0x51), BACKEND_PID, issued)
}

fn mock_duplicate(attempt: &DuplicateHandleAttempt) -> DuplicateHandleResult {
    Ok(DuplicateHandleSuccess::new(
        WindowsHandleValue::new(0xB0B),
        attempt.backend_pid,
        attempt.handoff_token,
    ))
}

/// Build one connected in-process local-socket pair: `(broker, backend)`.
fn connected_pair(label: &str) -> (Stream, Stream) {
    let socket_name = unique_socket_name(label);
    let listener = bind_test_socket(&socket_name)
        .unwrap_or_else(|err| panic!("bind handoff wire socket {socket_name}: {err}"));
    let accept = thread::spawn(move || listener.accept().expect("accept backend side"));
    let name = local_socket_name(&socket_name).expect("local socket name");
    let broker_side = Stream::connect(name).expect("connect broker side");
    let backend_side = accept.join().expect("accept thread");
    cleanup_test_socket(&socket_name);
    (broker_side, backend_side)
}

fn wire_delivery(stream: Stream) -> WireHandoffDelivery<Stream> {
    WireHandoffDelivery::new_local_socket(
        stream,
        SERVICE,
        CORRELATION_ID,
        Instant::now() + Duration::from_secs(5),
    )
}

#[test]
fn await_backend_ack_honors_deadline_for_silent_peer() {
    assert_ack_read_honors_deadline("hw-ack-silent-deadline", false);
}

#[test]
fn await_backend_ack_honors_deadline_for_trickled_partial_frame() {
    assert_ack_read_honors_deadline("hw-ack-partial-deadline", true);
}

fn assert_ack_read_honors_deadline(label: &str, trickle: bool) {
    use std::io::Write as _;
    use std::sync::mpsc;

    let (broker_side, backend_side) = connected_pair(label);
    let delivery = wire_delivery(broker_side);
    let mut backend_side = Some(backend_side);
    let writer = if trickle {
        let mut writer_stream = backend_side.take().expect("backend stream available");
        writer_stream
            .write_all(&[running_process::broker::protocol::ENVELOPE_VERSION])
            .expect("write first partial ACK byte");
        Some(thread::spawn(move || {
            let remaining = [16, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8];
            for byte in remaining {
                thread::sleep(Duration::from_millis(25));
                if writer_stream.write_all(&[byte]).is_err() {
                    break;
                }
            }
        }))
    } else {
        None
    };
    let expected = token(0x61);
    let (result_tx, result_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut delivery = delivery;
        let deadline = Instant::now() + Duration::from_millis(100);
        let started = Instant::now();
        let result = delivery.await_backend_ack(&expected, deadline);
        let _ = result_tx.send((result, started.elapsed()));
    });

    let received = result_rx.recv_timeout(Duration::from_millis(500));
    if received.is_err() {
        drop(backend_side.take());
        if let Some(writer) = writer {
            writer.join().expect("trickle writer joins");
        }
        let _ = result_rx.recv_timeout(Duration::from_secs(2));
        worker.join().expect("ACK worker joins after forced EOF");
        panic!("await_backend_ack ignored its caller deadline");
    }
    drop(backend_side.take());
    if let Some(writer) = writer {
        writer.join().expect("trickle writer joins");
    }
    worker.join().expect("ACK worker joins");
    let (result, elapsed) = received.expect("checked above");
    let error = result.expect_err("incomplete ACK must time out");

    assert!(
        elapsed >= Duration::from_millis(80) && elapsed < Duration::from_millis(500),
        "await_backend_ack did not return near its caller deadline: {elapsed:?}"
    );
    let HandoffDeliveryError::AckNotObserved { detail } = error else {
        panic!("expected AckNotObserved, got {error:?}");
    };
    assert!(
        detail.contains("timed out"),
        "timeout must remain distinct from EOF/malformed errors: {detail}"
    );
}

#[test]
fn serve_handoff_offer_with_deadline_times_out_a_silent_peer() {
    assert_offer_read_honors_deadline("hw-offer-silent-deadline", OfferPeer::Silent);
}

#[test]
fn serve_handoff_offer_with_deadline_times_out_a_partial_peer() {
    assert_offer_read_honors_deadline("hw-offer-partial-deadline", OfferPeer::Partial);
}

#[test]
fn serve_handoff_offer_with_deadline_stops_a_continuous_trickle() {
    assert_offer_read_honors_deadline("hw-offer-trickle-deadline", OfferPeer::Trickle);
}

enum OfferPeer {
    Silent,
    Partial,
    Trickle,
}

fn assert_offer_read_honors_deadline(label: &str, peer: OfferPeer) {
    use std::io::Write as _;

    let (mut broker_side, mut backend_side) = connected_pair(label);
    let writer = match peer {
        OfferPeer::Silent => None,
        OfferPeer::Partial => {
            // Send the prefix before the read starts, not from the spawned
            // thread.
            //
            // Spawning first raced the reader's teardown: the read times out
            // after 30ms and drops its end, so a writer thread scheduled late
            // hit `BrokenPipe: The pipe is being closed` and panicked on the
            // `expect`. Seen on a loaded machine during a parallel sweep.
            //
            // Tolerating the error instead would have hidden the race and
            // quietly turned this into the `Silent` case — the partial prefix
            // that gives this test its name would never reach the reader.
            // Writing synchronously guarantees it does; the thread then only
            // has to keep the connection open past the deadline.
            broker_side
                .write_all(&[1, 0])
                .expect("partial offer prefix");
            Some(thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                drop(broker_side);
            }))
        }
        OfferPeer::Trickle => Some(thread::spawn(move || {
            let offer = HandoffOffer {
                handle_value: 0xB0B,
                token: token(0x71).as_bytes().to_vec(),
                service_name: SERVICE.into(),
                correlation_id: CORRELATION_ID,
            };
            let frame = handoff_offer_frame(&offer);
            let mut frame_bytes = Vec::new();
            frame.encode(&mut frame_bytes).unwrap();
            let mut wire = Vec::new();
            write_frame(&mut wire, &frame_bytes).unwrap();
            for byte in wire {
                if broker_side.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(8));
            }
        })),
    };

    let now = Instant::now();
    let mut pending_tokens = HandoffTokenStore::new();
    let expected = issue_token(&mut pending_tokens, now, 0x71);
    let started = Instant::now();
    let error = serve_handoff_offer_with_deadline(
        &mut backend_side,
        &mut pending_tokens,
        expected,
        now,
        started + Duration::from_millis(30),
    )
    .expect_err("incomplete offer must time out");

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "offer read exceeded its public deadline bound"
    );
    assert!(
        matches!(
            error,
            BackendHandoffWireError::Framing(FramingError::Io(ref error))
                if error.kind() == io::ErrorKind::TimedOut
        ),
        "unexpected deadline error: {error:?}"
    );
    assert_eq!(
        pending_tokens.pending_len(),
        1,
        "timed-out offers must not consume the expected token"
    );

    drop(backend_side);
    if let Some(writer) = writer {
        writer.join().expect("offer peer");
    }
}

fn bind_test_socket(socket_name: &str) -> io::Result<interprocess::local_socket::Listener> {
    #[cfg(unix)]
    {
        let path = std::path::Path::new(socket_name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let _ = std::fs::remove_file(path);
    }
    let name = local_socket_name(socket_name)?;
    ListenerOptions::new().name(name).create_sync()
}

fn cleanup_test_socket(socket_name: &str) {
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(socket_name);
    }

    #[cfg(windows)]
    let _ = socket_name;
}

fn assert_fallback(
    outcome: &WindowsHandoffOutcome,
    expected_stage: WindowsHandoffStage,
    tokens: &HandoffTokenStore,
    acks: &HandoffAckRegistry,
) {
    let fallback = outcome.fallback().expect("expected reconnect fallback");
    assert_eq!(fallback.stage, expected_stage, "stage: {}", fallback.detail);
    assert!(fallback.decision.uses_backend_reconnect());
    assert!(!fallback.decision.sends_client_error());
    assert_eq!(tokens.pending_len(), 0, "one-time token must be revoked");
    assert_eq!(acks.pending_len(), 0, "pending ACK entry must be removed");
}

// ---------------------------------------------------------------------------
// Frame-level encode/decode round-trips for the new envelope messages.
// ---------------------------------------------------------------------------

#[test]
fn handoff_offer_frame_roundtrips_through_framing() {
    let offer = HandoffOffer {
        handle_value: 0xB0B,
        token: token(0x11).as_bytes().to_vec(),
        service_name: SERVICE.into(),
        correlation_id: CORRELATION_ID,
    };
    let frame = handoff_offer_frame(&offer);
    assert_eq!(frame.envelope_version, 1);
    assert_eq!(FrameKind::try_from(frame.kind), Ok(FrameKind::Request));
    assert_eq!(frame.payload_protocol, HANDOFF_PAYLOAD_PROTOCOL);
    assert_eq!(frame.request_id, CORRELATION_ID);
    assert_eq!(
        PayloadEncoding::try_from(frame.payload_encoding),
        Ok(PayloadEncoding::None)
    );

    let mut wire = Vec::new();
    let mut frame_bytes = Vec::new();
    frame.encode(&mut frame_bytes).unwrap();
    write_frame(&mut wire, &frame_bytes).unwrap();
    let read_back = read_frame(&mut wire.as_slice()).unwrap();
    let frame_back = Frame::decode(read_back.as_slice()).unwrap();
    assert_eq!(frame_back, frame);
    let offer_back = HandoffOffer::decode(frame_back.payload.as_slice()).unwrap();
    assert_eq!(offer_back, offer);
}

#[test]
fn handoff_ack_frame_roundtrips_through_framing() {
    let ack = HandoffAck {
        token: token(0x22).as_bytes().to_vec(),
        accepted: false,
        error_detail: "handoff token mismatch".into(),
        correlation_id: CORRELATION_ID,
    };
    let frame = handoff_ack_frame(&ack);
    assert_eq!(frame.envelope_version, 1);
    assert_eq!(FrameKind::try_from(frame.kind), Ok(FrameKind::Response));
    assert_eq!(frame.payload_protocol, HANDOFF_PAYLOAD_PROTOCOL);
    assert_eq!(frame.request_id, CORRELATION_ID);

    let mut wire = Vec::new();
    let mut frame_bytes = Vec::new();
    frame.encode(&mut frame_bytes).unwrap();
    write_frame(&mut wire, &frame_bytes).unwrap();
    let read_back = read_frame(&mut wire.as_slice()).unwrap();
    let frame_back = Frame::decode(read_back.as_slice()).unwrap();
    assert_eq!(frame_back, frame);
    let ack_back = HandoffAck::decode(frame_back.payload.as_slice()).unwrap();
    assert_eq!(ack_back, ack);
}

// ---------------------------------------------------------------------------
// End-to-end round trip over an in-process local-socket pair.
// ---------------------------------------------------------------------------

#[test]
fn wire_delivery_completes_orchestration_and_consumes_token_once() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x31);

    let (broker_side, backend_side) = connected_pair("hw-complete");
    let backend = thread::spawn(move || {
        let mut stream = backend_side;
        // The backend learned the expected token out of band (spawn env);
        // model that as its own pending store holding the same bytes.
        let backend_now = Instant::now();
        let mut backend_tokens = HandoffTokenStore::new();
        let expected = issue_token(&mut backend_tokens, backend_now, 0x31);
        let acceptance = serve_handoff_offer_with_deadline(
            &mut stream,
            &mut backend_tokens,
            expected,
            backend_now,
            backend_now + Duration::from_secs(5),
        )
        .expect("serve handoff offer");
        (acceptance, backend_tokens.pending_len())
    });

    let mut delivery = wire_delivery(broker_side);
    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        mock_duplicate,
        &mut delivery,
    );

    let (acceptance, backend_pending) = backend.join().expect("backend thread");
    assert!(
        outcome.is_completed(),
        "expected completed handoff, got {outcome:?}"
    );
    assert_eq!(
        tokens.pending_len(),
        0,
        "broker token consumed exactly once"
    );
    assert_eq!(acks.pending_len(), 0, "pending ACK entry completed");
    assert_eq!(backend_pending, 0, "backend token consumed exactly once");

    let HandoffAcceptance::Accepted(accepted) = acceptance else {
        panic!("backend must accept the offer");
    };
    assert_eq!(accepted.token, issued);
    assert_eq!(accepted.connection.handle_value, 0xB0B);
    assert_eq!(accepted.connection.service_name, SERVICE);
    assert_eq!(accepted.connection.correlation_id, CORRELATION_ID);
}

#[test]
fn refused_ack_falls_back_and_revokes_token() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x41);

    let (broker_side, backend_side) = connected_pair("hw-refused");
    let backend = thread::spawn(move || {
        let mut stream = backend_side;
        // Backend expects a DIFFERENT token, so acceptance is refused.
        let backend_now = Instant::now();
        let mut backend_tokens = HandoffTokenStore::new();
        let expected = issue_token(&mut backend_tokens, backend_now, 0x42);
        serve_handoff_offer_with_deadline(
            &mut stream,
            &mut backend_tokens,
            expected,
            backend_now,
            backend_now + Duration::from_secs(5),
        )
        .expect("serve handoff offer")
    });

    let mut delivery = wire_delivery(broker_side);
    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        mock_duplicate,
        &mut delivery,
    );

    let acceptance = backend.join().expect("backend thread");
    let HandoffAcceptance::Rejected(rejected) = acceptance else {
        panic!("backend must reject the mismatched offer");
    };
    assert_eq!(rejected.reason, HandoffRejectionReason::TokenMismatch);
    assert_fallback(&outcome, WindowsHandoffStage::AwaitAck, &tokens, &acks);
    let fallback = outcome.fallback().unwrap();
    assert!(
        fallback.detail.contains("refused"),
        "detail should mention the refusal: {}",
        fallback.detail
    );
}

#[test]
fn wrong_token_echo_in_ack_falls_back() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x51);

    let (broker_side, backend_side) = connected_pair("hw-bad-token");
    let backend = thread::spawn(move || {
        let mut stream = backend_side;
        let offer = read_handoff_offer(&mut stream).expect("read offer");
        let forged = HandoffAck {
            token: vec![0xFF; 16],
            accepted: true,
            error_detail: String::new(),
            correlation_id: offer.correlation_id,
        };
        write_handoff_ack(&mut stream, &forged).expect("write forged ack");
    });

    let mut delivery = wire_delivery(broker_side);
    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        mock_duplicate,
        &mut delivery,
    );

    backend.join().expect("backend thread");
    assert_fallback(&outcome, WindowsHandoffStage::AwaitAck, &tokens, &acks);
}

#[test]
fn wrong_correlation_id_in_ack_falls_back() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x61);

    let (broker_side, backend_side) = connected_pair("hw-bad-corr");
    let backend = thread::spawn(move || {
        let mut stream = backend_side;
        let offer = read_handoff_offer(&mut stream).expect("read offer");
        // Correct token echo, wrong correlation id at the payload level
        // (frame request_id forced to the expected value so the payload
        // check is the one that fires).
        let forged = HandoffAck {
            token: offer.token.clone(),
            accepted: true,
            error_detail: String::new(),
            correlation_id: offer.correlation_id + 1,
        };
        let mut frame = handoff_ack_frame(&forged);
        frame.request_id = offer.correlation_id;
        let mut bytes = Vec::new();
        frame.encode(&mut bytes).unwrap();
        write_frame(&mut stream, &bytes).expect("write forged ack frame");
    });

    let mut delivery = wire_delivery(broker_side);
    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        mock_duplicate,
        &mut delivery,
    );

    backend.join().expect("backend thread");
    assert_fallback(&outcome, WindowsHandoffStage::AwaitAck, &tokens, &acks);
}

#[test]
fn malformed_ack_frame_falls_back() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x71);

    let (broker_side, backend_side) = connected_pair("hw-malformed");
    let backend = thread::spawn(move || {
        let mut stream = backend_side;
        let offer = read_handoff_offer(&mut stream).expect("read offer");
        // Wrong payload protocol: a well-framed but non-handoff frame.
        let frame = Frame {
            envelope_version: 1,
            kind: FrameKind::Response as i32,
            payload_protocol: 0xAD01,
            payload: Vec::new(),
            request_id: offer.correlation_id,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        };
        let mut bytes = Vec::new();
        frame.encode(&mut bytes).unwrap();
        write_frame(&mut stream, &bytes).expect("write malformed frame");
    });

    let mut delivery = wire_delivery(broker_side);
    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        mock_duplicate,
        &mut delivery,
    );

    backend.join().expect("backend thread");
    assert_fallback(&outcome, WindowsHandoffStage::AwaitAck, &tokens, &acks);
}

#[test]
fn backend_disconnect_before_ack_falls_back() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x81);

    let (broker_side, backend_side) = connected_pair("hw-eof");
    let backend = thread::spawn(move || {
        let mut stream = backend_side;
        let _offer = read_handoff_offer(&mut stream).expect("read offer");
        // Drop the stream without acking: the broker observes EOF.
    });

    let mut delivery = wire_delivery(broker_side);
    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        mock_duplicate,
        &mut delivery,
    );

    backend.join().expect("backend thread");
    assert_fallback(&outcome, WindowsHandoffStage::AwaitAck, &tokens, &acks);
}

#[test]
fn ack_after_deadline_falls_back_and_revokes_token() {
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    // 1 ms ACK deadline: the backend's deliberate delay overshoots it.
    let mut acks = HandoffAckRegistry::with_ack_deadline(Duration::from_millis(1));
    let issued = issue_registered_token(&mut tokens, &mut acks, now, 0x91);

    let (broker_side, backend_side) = connected_pair("hw-late");
    let backend = thread::spawn(move || {
        let mut stream = backend_side;
        let backend_now = Instant::now();
        let mut backend_tokens = HandoffTokenStore::new();
        let expected = issue_token(&mut backend_tokens, backend_now, 0x91);
        let offer = read_handoff_offer(&mut stream).expect("read offer");
        thread::sleep(Duration::from_millis(50));
        running_process::broker::backend_lib::respond_to_handoff_offer(
            &mut stream,
            &mut backend_tokens,
            expected,
            offer,
            Instant::now(),
        )
        .expect("respond to offer");
    });

    let mut delivery = wire_delivery(broker_side);
    let outcome = execute_windows_handoff_with_transport(
        &mut tokens,
        &mut acks,
        &request(issued),
        mock_duplicate,
        &mut delivery,
    );

    backend.join().expect("backend thread");
    assert_fallback(&outcome, WindowsHandoffStage::AwaitAck, &tokens, &acks);
}

// ---------------------------------------------------------------------------
// Backend-side offer reader validation.
// ---------------------------------------------------------------------------

#[test]
fn read_handoff_offer_rejects_non_handoff_frames() {
    // A control-plane Hello-style frame must not parse as a handoff offer.
    let frame = Frame {
        envelope_version: 1,
        kind: FrameKind::Request as i32,
        payload_protocol: 0,
        payload: Vec::new(),
        request_id: 1,
        payload_encoding: PayloadEncoding::None as i32,
        deadline_unix_ms: 0,
        traceparent: String::new(),
        tracestate: String::new(),
    };
    let mut bytes = Vec::new();
    frame.encode(&mut bytes).unwrap();
    let mut wire = Vec::new();
    write_frame(&mut wire, &bytes).unwrap();

    let err = read_handoff_offer(&mut wire.as_slice()).expect_err("must reject");
    let BackendHandoffWireError::UnexpectedFrame(detail) = err else {
        panic!("expected UnexpectedFrame, got another error kind");
    };
    assert_eq!(detail, "payload_protocol is not handoff");
}

#[test]
fn read_handoff_offer_rejects_mismatched_request_id() {
    let offer = HandoffOffer {
        handle_value: 1,
        token: token(0xA1).as_bytes().to_vec(),
        service_name: SERVICE.into(),
        correlation_id: CORRELATION_ID,
    };
    let mut frame = handoff_offer_frame(&offer);
    frame.request_id = CORRELATION_ID + 1;
    let mut bytes = Vec::new();
    frame.encode(&mut bytes).unwrap();
    let mut wire = Vec::new();
    write_frame(&mut wire, &bytes).unwrap();

    let err = read_handoff_offer(&mut wire.as_slice()).expect_err("must reject");
    let BackendHandoffWireError::UnexpectedFrame(detail) = err else {
        panic!("expected UnexpectedFrame, got another error kind");
    };
    assert_eq!(
        detail,
        "frame request_id does not match HandoffOffer correlation_id"
    );
}
