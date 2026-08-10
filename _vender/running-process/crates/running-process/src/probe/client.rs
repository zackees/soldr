//! Transport for the probe client.
//!
//! One request/reply round trip per call over the daemon's framed control
//! socket. Every operation is deadline-bounded: a daemon that has bound its
//! socket but stopped accepting must not be able to wedge the calling
//! application, which is the failure mode an unbounded blocking read invites.

use std::io;
use std::time::Duration;

use prost::Message as _;
use running_process_probe::probe_diag::v1::{
    probe_envelope::Body, CaptureReply, CaptureStackRequest, Heartbeat, ProbeEnvelope, ProcessKey,
    RegisterProcess, RegistrationStatus, UnregisterProcess,
};

use crate::broker::protocol::framing::{read_frame_with_cap, write_frame, MAX_FRAME_BYTES};

/// Cap on a single reply. Registration replies are small; anything larger is a
/// malformed or hostile peer, and the cap bounds the allocation before it
/// happens.
const MAX_REPLY_BYTES: usize = 64 * 1024;

/// Why a client operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The daemon could not be reached.
    #[error("probe daemon unreachable: {0}")]
    Unreachable(#[source] io::Error),
    /// Wire-level failure (framing, encode, decode).
    #[error("probe wire error: {0}")]
    Wire(String),
    /// The daemon refused the request.
    #[error("probe daemon refused the request: {reason}")]
    Refused {
        /// Human-readable reason from the daemon.
        reason: String,
    },
    /// The daemon replied with something other than the expected message.
    #[error("unexpected reply from probe daemon")]
    UnexpectedReply,
    /// A reply arrived that answers a different request.
    ///
    /// Distinct from [`ClientError::Wire`] because it means the stream is
    /// still perfectly well-framed but no longer aligned — every subsequent
    /// reply on it would answer the wrong request. The connection cannot be
    /// reused, only rebuilt.
    #[error("probe daemon reply is out of order: expected request {expected}, got {got}")]
    Desync {
        /// The request id that was sent.
        expected: u64,
        /// The request id the received frame claimed to answer.
        got: u64,
    },
}

/// Work returned by the daemon while the probe is heartbeating.
#[derive(Clone, Debug, PartialEq)]
pub enum HeartbeatWork {
    /// No operation is waiting.
    Idle,
    /// Capture this process cooperatively and return the raw artifact.
    Capture(CaptureStackRequest),
}

/// The operations a probe client performs.
///
/// A trait so tests can drive the worker with an in-memory fake, and so a
/// later slice can supply a different transport without touching the worker's
/// reconnect and heartbeat logic.
pub trait ProbeClient: Send {
    /// Enroll this process; returns the identity the daemon armed.
    fn register(&mut self, req: &RegisterProcess) -> Result<ProcessKey, ClientError>;
    /// Refresh liveness.
    fn heartbeat(&mut self, key: &ProcessKey) -> Result<HeartbeatWork, ClientError>;
    /// Return the result of work leased on this connection.
    fn submit_capture(&mut self, reply: CaptureReply) -> Result<(), ClientError>;
    /// Best-effort deregistration.
    fn unregister(&mut self, key: &ProcessKey) -> Result<(), ClientError>;
}

/// A [`ProbeClient`] over the daemon's local control socket.
#[derive(Debug)]
pub struct SocketProbeClient {
    stream: interprocess::local_socket::Stream,
    request_id: u64,
}

impl SocketProbeClient {
    /// Connect to the daemon at `socket_path`, bounding the attempt by
    /// `deadline`.
    pub fn connect(socket_path: &str, deadline: Duration) -> Result<Self, ClientError> {
        use interprocess::local_socket::traits::Stream as _;

        let name = crate::broker::server::local_socket_name(socket_path)
            .map_err(|e| ClientError::Wire(format!("socket name: {e}")))?;
        let stream =
            interprocess::local_socket::Stream::connect(name).map_err(ClientError::Unreachable)?;

        // Bound receives. Without this a daemon that accepts and then stalls
        // would hold the worker thread forever. interprocess exposes only a
        // recv timeout; the send side is bounded in practice because requests
        // are small and the daemon reads promptly.
        stream
            .set_recv_timeout(Some(deadline))
            .map_err(ClientError::Unreachable)?;

        Ok(Self {
            stream,
            request_id: 0,
        })
    }

    fn next_request_id(&mut self) -> u64 {
        self.request_id = self.request_id.wrapping_add(1);
        self.request_id
    }

    fn round_trip(&mut self, body: Body) -> Result<ProbeEnvelope, ClientError> {
        let request_id = self.next_request_id();
        round_trip_on(&mut self.stream, request_id, body)
    }
}

/// Send one request and read the reply that answers it.
///
/// Split from [`SocketProbeClient`] so the correlation rule below can be
/// tested over an in-memory stream. Bound to a socket it could only be
/// exercised against a live daemon, which is exactly the wrong place to
/// verify a desync guard.
fn round_trip_on<S: io::Read + io::Write>(
    stream: &mut S,
    request_id: u64,
    body: Body,
) -> Result<ProbeEnvelope, ClientError> {
    let envelope = ProbeEnvelope {
        wire_version: 1,
        request_id,
        deadline_unix_ms: 0,
        body: Some(body),
    };

    write_frame(stream, &envelope.encode_to_vec()).map_err(|e| ClientError::Wire(e.to_string()))?;

    let bytes = read_frame_with_cap(stream, MAX_REPLY_BYTES.min(MAX_FRAME_BYTES))
        .map_err(|e| ClientError::Wire(e.to_string()))?;

    let reply = ProbeEnvelope::decode(bytes.as_slice())
        .map_err(|e| ClientError::Wire(format!("decode reply: {e}")))?;

    // The next frame on this socket is not necessarily *this* request's
    // reply. `request_id` exists to say which request a frame answers, and
    // until now it was written and never read back.
    //
    // That is harmless only while the daemon is a pure responder. The moment
    // it pushes anything unsolicited — the forwarded capture request #637
    // needs is the concrete case — the next `heartbeat` would read that push,
    // accept it as its own reply, and leave every later reply answering the
    // previous request. A silent, permanent off-by-one, on a channel whose
    // whole job is to report process state accurately.
    //
    // Failing loudly is also self-healing: the worker treats a heartbeat
    // error as a dead connection and re-runs the full register handshake, so
    // a desynced client resynchronizes instead of reporting stale data.
    if reply.request_id != request_id {
        return Err(ClientError::Desync {
            expected: request_id,
            got: reply.request_id,
        });
    }

    Ok(reply)
}

impl ProbeClient for SocketProbeClient {
    fn register(&mut self, req: &RegisterProcess) -> Result<ProcessKey, ClientError> {
        let reply = self.round_trip(Body::Register(req.clone()))?;
        match reply.body {
            Some(Body::RegistrationStatus(RegistrationStatus { state, detail, .. })) => {
                // 2 == ARMED in the probe_diag.v1 RegistrationStatus.State enum.
                if state == 2 {
                    req.key.clone().ok_or(ClientError::UnexpectedReply)
                } else {
                    Err(ClientError::Refused { reason: detail })
                }
            }
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    fn heartbeat(&mut self, key: &ProcessKey) -> Result<HeartbeatWork, ClientError> {
        let reply = self.round_trip(Body::Heartbeat(Heartbeat {
            key: Some(key.clone()),
        }))?;
        heartbeat_reply(reply)
    }

    fn submit_capture(&mut self, reply: CaptureReply) -> Result<(), ClientError> {
        let reply = self.round_trip(Body::CaptureReply(reply))?;
        match reply.body {
            Some(Body::RegistrationStatus(RegistrationStatus { error: 0, .. })) => Ok(()),
            Some(Body::RegistrationStatus(status)) => Err(ClientError::Refused {
                reason: status.detail,
            }),
            _ => Err(ClientError::UnexpectedReply),
        }
    }

    fn unregister(&mut self, key: &ProcessKey) -> Result<(), ClientError> {
        self.round_trip(Body::Unregister(UnregisterProcess {
            key: Some(key.clone()),
        }))?;
        Ok(())
    }
}

fn heartbeat_reply(reply: ProbeEnvelope) -> Result<HeartbeatWork, ClientError> {
    match reply.body {
        Some(Body::CaptureStack(request)) => Ok(HeartbeatWork::Capture(request)),
        Some(Body::RegistrationStatus(RegistrationStatus { error: 0, .. })) => {
            Ok(HeartbeatWork::Idle)
        }
        Some(Body::RegistrationStatus(status)) => Err(ClientError::Refused {
            reason: status.detail,
        }),
        _ => Err(ClientError::UnexpectedReply),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// In-memory client for driving the worker without a daemon.
    #[derive(Default)]
    pub(crate) struct FakeClient {
        pub registered: Arc<Mutex<u32>>,
        pub heartbeats: Arc<Mutex<u32>>,
        pub unregistered: Arc<Mutex<u32>>,
        pub fail_register: bool,
    }

    impl ProbeClient for FakeClient {
        fn register(&mut self, req: &RegisterProcess) -> Result<ProcessKey, ClientError> {
            if self.fail_register {
                return Err(ClientError::Refused {
                    reason: "test".into(),
                });
            }
            *self.registered.lock().unwrap() += 1;
            req.key.clone().ok_or(ClientError::UnexpectedReply)
        }
        fn heartbeat(&mut self, _key: &ProcessKey) -> Result<HeartbeatWork, ClientError> {
            *self.heartbeats.lock().unwrap() += 1;
            Ok(HeartbeatWork::Idle)
        }
        fn submit_capture(&mut self, _reply: CaptureReply) -> Result<(), ClientError> {
            Ok(())
        }
        fn unregister(&mut self, _key: &ProcessKey) -> Result<(), ClientError> {
            *self.unregistered.lock().unwrap() += 1;
            Ok(())
        }
    }

    #[test]
    fn connect_to_a_nonexistent_socket_is_unreachable_not_a_hang() {
        let err = SocketProbeClient::connect(
            if cfg!(windows) {
                r"\\.\pipe\rp-probe-definitely-not-bound-633"
            } else {
                "/tmp/rp-probe-definitely-not-bound-633.sock"
            },
            Duration::from_millis(100),
        )
        .expect_err("must not connect");
        assert!(matches!(err, ClientError::Unreachable(_)), "{err:?}");
    }

    #[test]
    fn fake_client_round_trips_for_worker_tests() {
        let mut c = FakeClient::default();
        let key = ProcessKey {
            pid: 1,
            start_time: Some(2),
            boot_id: Some("b".into()),
        };
        let req = RegisterProcess {
            key: Some(key.clone()),
            ..Default::default()
        };
        assert_eq!(c.register(&req).unwrap(), key);
        assert_eq!(c.heartbeat(&key).unwrap(), HeartbeatWork::Idle);
        c.unregister(&key).unwrap();
        assert_eq!(*c.registered.lock().unwrap(), 1);
        assert_eq!(*c.heartbeats.lock().unwrap(), 1);
        assert_eq!(*c.unregistered.lock().unwrap(), 1);
    }

    /// A stream preloaded with the frames the daemon "sends", recording what
    /// the client wrote.
    struct Duplex {
        incoming: io::Cursor<Vec<u8>>,
        written: Vec<u8>,
    }

    impl io::Read for Duplex {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.incoming.read(buf)
        }
    }

    impl io::Write for Duplex {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.written.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Build a stream that will hand back `frames` in order.
    fn daemon_sending(frames: &[ProbeEnvelope]) -> Duplex {
        let mut bytes = Vec::new();
        for frame in frames {
            write_frame(&mut bytes, &frame.encode_to_vec()).unwrap();
        }
        Duplex {
            incoming: io::Cursor::new(bytes),
            written: Vec::new(),
        }
    }

    /// A `RegistrationStatus` reply claiming to answer `request_id`.
    fn reply_to(request_id: u64) -> ProbeEnvelope {
        ProbeEnvelope {
            wire_version: 1,
            request_id,
            deadline_unix_ms: 0,
            body: Some(Body::RegistrationStatus(RegistrationStatus::default())),
        }
    }

    fn heartbeat_body() -> Body {
        Body::Heartbeat(Heartbeat::default())
    }

    /// #637: a heartbeat reply may carry work pushed by the daemon. It must
    /// reach the probe worker rather than being accepted as an ordinary ack.
    #[test]
    fn a_capture_push_is_returned_to_the_probe_worker() {
        let capture = running_process_probe::probe_diag::v1::CaptureStackRequest {
            max_depth: 64,
            thread_filter: 0,
            ..Default::default()
        };
        let reply = ProbeEnvelope {
            wire_version: 1,
            request_id: 7,
            deadline_unix_ms: 0,
            body: Some(Body::CaptureStack(capture.clone())),
        };

        assert_eq!(
            heartbeat_reply(reply).expect("capture reply"),
            HeartbeatWork::Capture(capture)
        );
    }

    #[test]
    fn a_reply_that_answers_the_request_is_accepted() {
        let mut stream = daemon_sending(&[reply_to(7)]);
        let reply = round_trip_on(&mut stream, 7, heartbeat_body()).expect("matching id");
        assert_eq!(reply.request_id, 7);
    }

    /// The request id must actually be sent, not merely compared against.
    /// A guard that checked a number the daemon never received would reject
    /// every well-behaved reply.
    #[test]
    fn the_request_carries_the_id_the_reply_is_matched_against() {
        let mut stream = daemon_sending(&[reply_to(42)]);
        round_trip_on(&mut stream, 42, heartbeat_body()).unwrap();

        let mut sent = io::Cursor::new(stream.written);
        let frame = read_frame_with_cap(&mut sent, MAX_REPLY_BYTES).expect("a request was written");
        let envelope = ProbeEnvelope::decode(frame.as_slice()).unwrap();
        assert_eq!(envelope.request_id, 42);
    }

    #[test]
    fn a_reply_answering_a_different_request_is_refused() {
        let mut stream = daemon_sending(&[reply_to(2)]);
        match round_trip_on(&mut stream, 1, heartbeat_body()) {
            Err(ClientError::Desync { expected, got }) => {
                assert_eq!((expected, got), (1, 2));
            }
            other => panic!("expected a desync error, got {other:?}"),
        }
    }

    /// The case this guard exists for (#637).
    ///
    /// The daemon pushes an unsolicited frame — a forwarded capture request —
    /// and only then the heartbeat reply. Without correlation the client
    /// accepts the push as its heartbeat reply and every later reply answers
    /// the previous request, silently and permanently.
    #[test]
    fn an_unsolicited_push_is_not_mistaken_for_the_reply() {
        // request_id 0 is what a server-initiated frame carries: it answers
        // no request of ours.
        let push = ProbeEnvelope {
            wire_version: 1,
            request_id: 0,
            deadline_unix_ms: 0,
            body: Some(Body::Heartbeat(Heartbeat::default())),
        };
        let mut stream = daemon_sending(&[push, reply_to(1)]);

        let outcome = round_trip_on(&mut stream, 1, heartbeat_body());
        assert!(
            matches!(outcome, Err(ClientError::Desync { .. })),
            "a pushed frame must not be consumed as this request's reply, got {outcome:?}"
        );
    }

    /// Desync must be distinguishable from a framing failure: one means the
    /// stream is broken, the other that it is intact but misaligned. Both are
    /// fatal to the connection, but only one indicates a protocol bug.
    #[test]
    fn desync_is_not_reported_as_a_wire_error() {
        let mut stream = daemon_sending(&[reply_to(9)]);
        let err = round_trip_on(&mut stream, 8, heartbeat_body()).unwrap_err();
        assert!(!matches!(err, ClientError::Wire(_)), "got {err:?}");
        assert!(
            err.to_string().contains("out of order"),
            "the message should say what went wrong: {err}"
        );
    }
}
