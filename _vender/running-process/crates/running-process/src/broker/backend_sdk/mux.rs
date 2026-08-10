//! Sans-io endpoint multiplexer for backend daemons (#412).
//!
//! A consumer daemon's IPC endpoint serves three kinds of traffic:
//!
//! 1. its own **legacy wire** (whatever framing the daemon spoke before
//!    adopting running-process),
//! 2. **`BackendHandle` identity probes** (nonce challenge frames sent
//!    by [`crate::broker::backend_handle::BackendHandle::probe_with_service`]),
//! 3. **consumer payload frames** — the daemon's own requests carried
//!    opaquely in v1 `Frame` envelopes under a registered payload
//!    protocol.
//!
//! Before #412, every consumer hand-rolled the byte-level
//! disambiguation (including the genuinely ambiguous case where a
//! legacy length header makes byte 0 equal the v1 framing byte), probe
//! validation, and probe-response construction. `BackendEndpointMux`
//! owns all of it as a pure function of the read buffer: the daemon
//! keeps its own sockets, runtime, and buffered reads, and calls
//! [`BackendEndpointMux::poll`] whenever bytes arrive.

use crate::broker::backend_lifecycle::identity::DaemonProcess;
use crate::broker::backend_lifecycle::probe::{
    endpoint_probe_request_from_frame, endpoint_probe_response_frame, EndpointProbeServerError,
};
use crate::broker::protocol::{
    encode_framed, registry, try_decode_framed, Frame, FrameKind, FramingError, ENVELOPE_VERSION,
};

/// Consumer verdict on whether buffered bytes belong to its legacy wire.
///
/// The detector runs **before** any frame decoding, so a legacy header
/// whose first byte happens to equal the v1 framing byte
/// ([`ENVELOPE_VERSION`]) is classified by the consumer, which knows
/// its own header layout, not by running-process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyClassification {
    /// The bytes start a legacy-wire message; the mux steps aside.
    Legacy,
    /// The bytes are not legacy wire; the mux continues frame handling.
    NotLegacy,
    /// Too few bytes to decide; read more and poll again.
    NeedMoreBytes,
}

/// What the daemon's accept loop should do with its buffered bytes.
#[derive(Debug)]
pub enum MuxPoll {
    /// Not enough bytes to classify or to complete a frame. Read more
    /// bytes into the buffer and poll again. Nothing is consumed.
    NeedMoreBytes,
    /// The buffer starts a legacy-wire message. Nothing is consumed;
    /// hand the buffer to the daemon's legacy decoder.
    Legacy,
    /// A `BackendHandle` identity probe was answered. Write `reply` to
    /// the peer verbatim, then advance the read buffer by `consumed`.
    ProbeAnswered {
        /// Complete wire bytes (`[1][len][prost Frame]`) to send back.
        reply: Vec<u8>,
        /// Bytes of the probe request to consume from the buffer front.
        consumed: usize,
    },
    /// A consumer payload frame arrived. Dispatch `frame.payload`
    /// through the daemon's request handler (correlate on
    /// `frame.request_id`), then advance the read buffer by `consumed`.
    Payload {
        /// The decoded consumer frame (kind, payload protocol, payload,
        /// request id, trace context).
        frame: Frame,
        /// Bytes the frame occupied; consume from the buffer front.
        consumed: usize,
    },
}

/// Errors surfaced by [`BackendEndpointMux::poll`].
///
/// All of them mean the connection is in an unknown state; the daemon
/// should drop it (matching broker behavior for framing violations).
#[derive(Debug, thiserror::Error)]
pub enum MuxError {
    /// Outer framing violation (oversize body, undecodable frame).
    #[error(transparent)]
    Framing(#[from] FramingError),
    /// A frame with the probe payload protocol failed probe validation.
    #[error("malformed BackendHandle probe: {0}")]
    MalformedProbe(#[from] EndpointProbeServerError),
    /// A frame carried a first-party payload protocol this endpoint
    /// does not serve (e.g. broker Hello sent to a backend daemon).
    #[error(
        "unexpected first-party frame on backend endpoint \
         (payload_protocol {payload_protocol:#06X})"
    )]
    UnexpectedFirstPartyFrame {
        /// The first-party payload protocol the peer used.
        payload_protocol: u32,
    },
    /// A frame carried a payload protocol other than the ones this mux
    /// was configured to accept.
    #[error("frame for unserved payload protocol {payload_protocol:#06X}")]
    UnservedPayloadProtocol {
        /// The payload protocol the peer used.
        payload_protocol: u32,
    },
}

/// Sans-io classifier for a backend daemon endpoint.
///
/// Construct one per daemon (it is cheap and immutable; sharing behind
/// an `Arc` is fine) with the daemon's own [`DaemonProcess`] identity,
/// the consumer payload protocols it serves, and a legacy detector
/// closure. In the accept loop, buffer reads as usual and call
/// [`Self::poll`]:
///
/// ```
/// use running_process::broker::backend_sdk::{
///     BackendEndpointMux, LegacyClassification, MuxPoll,
/// };
/// use running_process::broker::backend_handle::DaemonProcess;
/// use running_process::broker::protocol::Endpoint;
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let endpoint = Endpoint::unix_socket("my-daemon", "/tmp/my-daemon.sock")?;
/// let daemon = DaemonProcess::current_process(endpoint, None)?;
/// let mux = BackendEndpointMux::new(daemon, &[0x7A63], |buf| {
///     // First byte of my legacy wire is always the ASCII 'L' magic.
///     match buf.first() {
///         None => LegacyClassification::NeedMoreBytes,
///         Some(b'L') => LegacyClassification::Legacy,
///         Some(_) => LegacyClassification::NotLegacy,
///     }
/// });
///
/// let mut read_buf: Vec<u8> = Vec::new();
/// // ... read bytes into read_buf, then:
/// match mux.poll(&read_buf)? {
///     MuxPoll::NeedMoreBytes => { /* read more */ }
///     MuxPoll::Legacy => { /* my own decoder takes over */ }
///     MuxPoll::ProbeAnswered { reply, consumed } => {
///         // write `reply`, then read_buf.drain(..consumed);
///     }
///     MuxPoll::Payload { frame, consumed } => {
///         // dispatch frame.payload, then read_buf.drain(..consumed);
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// Daemons with no legacy wire pass a detector that always returns
/// [`LegacyClassification::NotLegacy`].
pub struct BackendEndpointMux<F> {
    daemon: DaemonProcess,
    served_payload_protocols: Vec<u32>,
    legacy_detector: F,
}

impl<F> BackendEndpointMux<F>
where
    F: Fn(&[u8]) -> LegacyClassification,
{
    /// Build a mux for `daemon`, serving the given consumer payload
    /// protocols, with a consumer-provided legacy-wire detector.
    pub fn new(
        daemon: DaemonProcess,
        served_payload_protocols: &[u32],
        legacy_detector: F,
    ) -> Self {
        Self {
            daemon,
            served_payload_protocols: served_payload_protocols.to_vec(),
            legacy_detector,
        }
    }

    /// Classify the front of `buf` and, for probes, build the reply.
    ///
    /// Pure with respect to I/O: never reads or writes a socket and
    /// never consumes from `buf` — the returned `consumed` counts tell
    /// the caller how far to advance. See [`MuxPoll`] for the contract
    /// of each verdict and [`MuxError`] for connection-fatal outcomes.
    pub fn poll(&self, buf: &[u8]) -> Result<MuxPoll, MuxError> {
        if buf.is_empty() {
            return Ok(MuxPoll::NeedMoreBytes);
        }

        // The consumer's own wire wins ties: only it knows whether a
        // leading 0x01 is a legacy length byte or our framing byte.
        match (self.legacy_detector)(buf) {
            LegacyClassification::Legacy => return Ok(MuxPoll::Legacy),
            LegacyClassification::NeedMoreBytes => return Ok(MuxPoll::NeedMoreBytes),
            LegacyClassification::NotLegacy => {}
        }

        // Not legacy and not our framing byte: still the consumer's
        // problem (its decoder owns the error path for garbage).
        if buf[0] != ENVELOPE_VERSION {
            return Ok(MuxPoll::Legacy);
        }

        let Some(decoded) = try_decode_framed(buf)? else {
            return Ok(MuxPoll::NeedMoreBytes);
        };
        let frame = decoded.frame;
        let consumed = decoded.consumed;

        if frame.payload_protocol == registry::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL {
            let request = endpoint_probe_request_from_frame(&frame)?;
            let response = endpoint_probe_response_frame(&request, &self.daemon);
            let reply = encode_framed(&response)?;
            return Ok(MuxPoll::ProbeAnswered { reply, consumed });
        }

        if registry::is_first_party(frame.payload_protocol) {
            return Err(MuxError::UnexpectedFirstPartyFrame {
                payload_protocol: frame.payload_protocol,
            });
        }

        if !self
            .served_payload_protocols
            .contains(&frame.payload_protocol)
        {
            return Err(MuxError::UnservedPayloadProtocol {
                payload_protocol: frame.payload_protocol,
            });
        }

        // Cancel/event frames are reserved for v1.x; requests and
        // responses both pass through so daemons can also act as
        // frame clients on reused connections.
        let _ = FrameKind::try_from(frame.kind);
        Ok(MuxPoll::Payload { frame, consumed })
    }

    /// The daemon identity this mux answers probes with.
    pub fn daemon(&self) -> &DaemonProcess {
        &self.daemon
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::backend_lifecycle::probe::PROBE_NONCE_BYTES;
    use crate::broker::protocol::{registry, Endpoint, PayloadEncoding};
    use prost::Message;

    const TEST_PROTOCOL: u32 = 0x7001;

    fn test_daemon() -> DaemonProcess {
        let endpoint = Endpoint::unix_socket("mux-test", "/tmp/mux-test.sock").expect("endpoint");
        DaemonProcess::current_process(endpoint, Some(30)).expect("identity")
    }

    fn test_mux() -> BackendEndpointMux<impl Fn(&[u8]) -> LegacyClassification> {
        BackendEndpointMux::new(test_daemon(), &[TEST_PROTOCOL], |buf: &[u8]| {
            // Toy legacy wire: 4-byte LE length, then 4-byte LE version 15.
            if buf.len() < 8 {
                return LegacyClassification::NeedMoreBytes;
            }
            let version = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
            if version == 15 {
                LegacyClassification::Legacy
            } else {
                LegacyClassification::NotLegacy
            }
        })
    }

    fn probe_request_wire(nonce: [u8; PROBE_NONCE_BYTES]) -> Vec<u8> {
        let frame = Frame::request(
            registry::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
            nonce.to_vec(),
        )
        .with_request_id(7);
        encode_framed(&frame).expect("encode probe")
    }

    #[test]
    fn empty_and_short_buffers_need_more_bytes() {
        let mux = test_mux();
        assert!(matches!(mux.poll(&[]), Ok(MuxPoll::NeedMoreBytes)));
        // 0x01 leading byte is ambiguous until the detector can rule.
        assert!(matches!(mux.poll(&[1, 0, 0]), Ok(MuxPoll::NeedMoreBytes)));
    }

    #[test]
    fn legacy_header_wins_even_with_frame_version_first_byte() {
        let mux = test_mux();
        // Legacy message of length 0x...01 — byte 0 collides with the
        // v1 framing byte. The detector sees version 15 and claims it.
        let mut legacy = 257u32.to_le_bytes().to_vec();
        legacy.extend_from_slice(&15u32.to_le_bytes());
        assert_eq!(legacy[0], ENVELOPE_VERSION);
        assert!(matches!(mux.poll(&legacy), Ok(MuxPoll::Legacy)));

        // Non-frame first byte goes to the consumer too.
        assert!(matches!(
            mux.poll(&[42, 0, 0, 0, 0, 16, 0, 0, 0]),
            Ok(MuxPoll::Legacy)
        ));
    }

    #[test]
    fn probe_request_is_answered_with_identity_echo() {
        let mux = test_mux();
        let nonce = [9u8; PROBE_NONCE_BYTES];
        let wire = probe_request_wire(nonce);

        // Partial probe frames wait for more bytes.
        assert!(matches!(
            mux.poll(&wire[..wire.len() - 1]),
            Ok(MuxPoll::NeedMoreBytes)
        ));

        let MuxPoll::ProbeAnswered { reply, consumed } = mux.poll(&wire).expect("poll") else {
            panic!("expected ProbeAnswered");
        };
        assert_eq!(consumed, wire.len());

        let decoded = try_decode_framed(&reply)
            .expect("decode")
            .expect("complete");
        let frame = decoded.frame;
        assert_eq!(
            frame.payload_protocol,
            registry::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL
        );
        assert_eq!(frame.request_id, 7);
        assert_eq!(&frame.payload[..PROBE_NONCE_BYTES], &nonce);
        let identity =
            crate::broker::protocol::DaemonProcess::decode(&frame.payload[PROBE_NONCE_BYTES..])
                .expect("identity payload");
        assert_eq!(identity.pid, std::process::id());
    }

    #[test]
    fn consumer_payload_frame_passes_through() {
        let mux = test_mux();
        let request = Frame::request(TEST_PROTOCOL, b"ping".to_vec()).with_request_id(3);
        let wire = encode_framed(&request).expect("encode");
        let MuxPoll::Payload { frame, consumed } = mux.poll(&wire).expect("poll") else {
            panic!("expected Payload");
        };
        assert_eq!(frame, request);
        assert_eq!(consumed, wire.len());
    }

    #[test]
    fn first_party_and_unserved_protocols_are_rejected() {
        let mux = test_mux();
        // Payload bytes keep the wire ≥ 8 bytes so the toy legacy
        // detector can classify (a 7-byte frame is legitimately
        // ambiguous and yields NeedMoreBytes instead).
        let hello = Frame::request(registry::CONTROL_PAYLOAD_PROTOCOL, b"hello".to_vec());
        let wire = encode_framed(&hello).expect("encode");
        assert!(matches!(
            mux.poll(&wire),
            Err(MuxError::UnexpectedFirstPartyFrame {
                payload_protocol: 0
            })
        ));

        let other = Frame::request(0x7002, Vec::new());
        let wire = encode_framed(&other).expect("encode");
        assert!(matches!(
            mux.poll(&wire),
            Err(MuxError::UnservedPayloadProtocol {
                payload_protocol: 0x7002
            })
        ));
    }

    #[test]
    fn malformed_probe_is_connection_fatal() {
        let mux = test_mux();
        let mut bad = Frame::request(
            registry::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
            vec![0u8; PROBE_NONCE_BYTES - 1],
        );
        bad.payload_encoding = PayloadEncoding::None as i32;
        let wire = encode_framed(&bad).expect("encode");
        assert!(matches!(mux.poll(&wire), Err(MuxError::MalformedProbe(_))));
    }
}
