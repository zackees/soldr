//! Unix end-to-end test for the orchestrated `SCM_RIGHTS` handoff
//! (#354, slice 4).
//!
//! Mirrors the in-process receiver pattern from the `unix.rs` transport
//! tests: a real `UnixListener` backend handoff socket plus a receiver
//! thread (the "backend"). The broker side runs [`execute_unix_handoff`]
//! with the production `sendmsg(SCM_RIGHTS)` transport, passing one end of
//! a real `UnixStream::pair()` as the handed-off client connection. The
//! backend receives the descriptor and token, reads the client payload
//! through the received descriptor, and echoes the token back as the
//! acknowledgement.

#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::mpsc;
use std::thread;
use std::time::Instant;

use running_process::broker::backend_lib::{accept_handed_off, HandedOffPayload};
use running_process::broker::server::handoff::{
    execute_unix_handoff, HandoffAckError, HandoffAckRegistry, HandoffDeliveryError, HandoffToken,
    HandoffTokenStore, PendingHandoffBackend, UnixFileDescriptor, UnixHandoffAckWait,
    UnixHandoffOutcome, UnixHandoffRequest, UnixHandoffSocket,
};

const CLIENT_PAYLOAD: &[u8] = b"hello-through-handed-off-fd";

/// ACK observed by the broker once the backend has adopted the connection.
struct BackendEcho {
    token: HandoffToken,
    payload: Vec<u8>,
}

/// ACK channel fed by the in-process backend thread.
struct ChannelAckWait {
    receiver: mpsc::Receiver<BackendEcho>,
    observed: Option<BackendEcho>,
}

impl UnixHandoffAckWait for ChannelAckWait {
    fn await_backend_ack(
        &mut self,
        token: &HandoffToken,
        deadline: Instant,
    ) -> Result<Instant, HandoffDeliveryError> {
        let timeout = deadline.saturating_duration_since(Instant::now());
        let echo = self.receiver.recv_timeout(timeout).map_err(|err| {
            HandoffDeliveryError::AckNotObserved {
                detail: format!("backend echo not received: {err}"),
            }
        })?;
        if echo.token != *token {
            return Err(HandoffDeliveryError::AckNotObserved {
                detail: "backend echoed a different token".into(),
            });
        }
        self.observed = Some(echo);
        Ok(Instant::now())
    }
}

/// Receive one `SCM_RIGHTS` message: the 16-byte token plus one fd.
fn recv_fd_and_token(stream: &UnixStream) -> (RawFd, HandoffToken) {
    let mut token_payload = [0_u8; 16];
    let mut iov = libc::iovec {
        iov_base: token_payload.as_mut_ptr().cast(),
        iov_len: token_payload.len(),
    };
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as _) as usize };
    let mut control = vec![0_u8; space];
    let mut message = unsafe { std::mem::zeroed::<libc::msghdr>() };
    message.msg_iov = &mut iov;
    message.msg_iovlen = 1;
    message.msg_control = control.as_mut_ptr().cast();
    message.msg_controllen = control.len() as _;

    let received = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut message, 0) };
    assert_eq!(received as usize, token_payload.len());

    let header = unsafe { libc::CMSG_FIRSTHDR(&message) };
    assert!(!header.is_null());
    unsafe {
        assert_eq!((*header).cmsg_level, libc::SOL_SOCKET);
        assert_eq!((*header).cmsg_type, libc::SCM_RIGHTS);
        let received_fd = *libc::CMSG_DATA(header).cast::<libc::c_int>();
        (received_fd, HandoffToken::from_bytes(token_payload))
    }
}

#[test]
fn unix_e2e_handoff_passes_real_fd_and_completes_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let socket_path = dir.path().join("handoff-e2e.sock");
    let listener = UnixListener::bind(&socket_path).unwrap();

    // The "backend": accept the handoff connection, receive fd + token,
    // read the client payload through the received fd, echo the token.
    let (ack_tx, ack_rx) = mpsc::channel::<BackendEcho>();
    let backend = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let (received_fd, received_token) = recv_fd_and_token(&stream);
        // Adopt the received descriptor and read what the client wrote.
        let mut adopted = unsafe { UnixStream::from_raw_fd(received_fd) };
        let mut payload = vec![0_u8; CLIENT_PAYLOAD.len()];
        adopted.read_exact(&mut payload).unwrap();
        ack_tx
            .send(BackendEcho {
                token: received_token,
                payload,
            })
            .unwrap();
    });

    // The handed-off "client connection": the client end writes the
    // payload; the broker-held end is what gets passed to the backend.
    let (mut client_end, broker_held_conn) = UnixStream::pair().unwrap();
    client_end.write_all(CLIENT_PAYLOAD).unwrap();

    // Broker-side issuance: one-time token registered for an ACK.
    let now = Instant::now();
    let mut tokens = HandoffTokenStore::new();
    let mut acks = HandoffAckRegistry::new();
    let issued = tokens.issue(now).unwrap();
    acks.register(
        issued,
        PendingHandoffBackend::new("zccache", std::process::id()),
        now,
    );

    let mut ack_wait = ChannelAckWait {
        receiver: ack_rx,
        observed: None,
    };
    let outcome = execute_unix_handoff(
        &mut tokens,
        &mut acks,
        &UnixHandoffRequest::new(
            UnixFileDescriptor::new(broker_held_conn.as_raw_fd()),
            UnixHandoffSocket::new(&socket_path),
            issued,
        ),
        &mut ack_wait,
    );
    backend.join().unwrap();

    // Completed end to end with the real transport.
    let UnixHandoffOutcome::Completed(completed) = outcome else {
        panic!("expected completed handoff, got {outcome:?}");
    };
    assert_eq!(completed.sent.handoff_token, issued);
    assert_eq!(completed.acknowledged.token, issued);
    let echo = ack_wait.observed.expect("ACK wait observed the echo");
    assert_eq!(echo.token, issued);
    assert_eq!(
        echo.payload, CLIENT_PAYLOAD,
        "backend must read the client payload through the received fd"
    );

    // The broker still owns its descriptor and can close it now; SCM_RIGHTS
    // duplicated it into the backend, which already adopted and dropped it.
    drop(broker_held_conn);

    // Exactly-once token consumption: a second ACK and a backend-side
    // replay of the same token are both rejected.
    assert_eq!(tokens.pending_len(), 0);
    assert_eq!(acks.pending_len(), 0);
    assert_eq!(
        acks.acknowledge(&mut tokens, &issued, Instant::now()),
        Err(HandoffAckError::TokenNotPending)
    );
    let replay = accept_handed_off(
        &mut tokens,
        HandedOffPayload::new(issued, issued.as_bytes().to_vec(), "replayed-conn"),
        Instant::now(),
    );
    assert!(replay.is_rejected());
}
