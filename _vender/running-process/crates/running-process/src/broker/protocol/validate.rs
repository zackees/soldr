//! Shared v1 frame-envelope validation (#376).
//!
//! Client and broker validate the same four `Frame` envelope fields in
//! the same order before touching a payload: `envelope_version`, `kind`,
//! `payload_protocol`, `payload_encoding`. This module centralizes that
//! check once; each call site keeps its own error type by mapping the
//! neutral [`FrameValidationError`] onto its existing errors, so observable
//! behavior (error variants, refusal codes, message strings) is unchanged.
//!
//! The expected frame kind and payload protocol differ per call site
//! (client expects `RESPONSE` frames, the broker Hello path expects
//! `REQUEST`, the handoff relay expects `EVENT`), so both are explicit
//! parameters rather than baked-in defaults.

use crate::broker::protocol::registry::PROTOCOL_VERSION;
use crate::broker::protocol::{Frame, FrameKind, PayloadEncoding};

/// Neutral envelope-validation failure.
///
/// Carries the offending wire values so callers can render their existing
/// diagnostics; it deliberately does NOT pick an error mapping (client
/// `BrokerClientError` vs broker `Refused` vs handoff static strings differ
/// and must stay byte-for-byte identical to their pre-#376 behavior).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameValidationError {
    /// `Frame.envelope_version` is not [`PROTOCOL_VERSION`].
    EnvelopeVersion {
        /// Version found on the frame.
        actual: u32,
    },
    /// `Frame.kind` is not the expected kind for this exchange.
    Kind {
        /// Kind the call site requires.
        expected: FrameKind,
        /// Raw kind value found on the frame.
        actual: i32,
    },
    /// `Frame.payload_protocol` is not the expected payload protocol.
    PayloadProtocol {
        /// Payload protocol the call site requires.
        expected: u32,
        /// Payload protocol found on the frame.
        actual: u32,
    },
    /// `Frame.payload_encoding` is not `PAYLOAD_ENCODING_NONE`.
    PayloadEncoding {
        /// Raw encoding value found on the frame.
        actual: i32,
    },
}

/// Validate the four v1 envelope fields shared by every framed exchange.
///
/// Check order is fixed (version, kind, payload protocol, encoding) and
/// matches every pre-existing call site, so the first failure reported is
/// identical to the previous hand-rolled validators.
pub fn validate_frame_envelope(
    frame: &Frame,
    expected_kind: FrameKind,
    expected_payload_protocol: u32,
) -> Result<(), FrameValidationError> {
    if frame.envelope_version != PROTOCOL_VERSION {
        return Err(FrameValidationError::EnvelopeVersion {
            actual: frame.envelope_version,
        });
    }
    if FrameKind::try_from(frame.kind) != Ok(expected_kind) {
        return Err(FrameValidationError::Kind {
            expected: expected_kind,
            actual: frame.kind,
        });
    }
    if frame.payload_protocol != expected_payload_protocol {
        return Err(FrameValidationError::PayloadProtocol {
            expected: expected_payload_protocol,
            actual: frame.payload_protocol,
        });
    }
    if PayloadEncoding::try_from(frame.payload_encoding) != Ok(PayloadEncoding::None) {
        return Err(FrameValidationError::PayloadEncoding {
            actual: frame.payload_encoding,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_frame_envelope, FrameValidationError};
    use crate::broker::protocol::registry::{
        CONTROL_PAYLOAD_PROTOCOL, HANDOFF_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
    };
    use crate::broker::protocol::{Frame, FrameKind, PayloadEncoding};

    fn valid_frame(kind: FrameKind, payload_protocol: u32) -> Frame {
        Frame {
            envelope_version: PROTOCOL_VERSION,
            kind: kind as i32,
            payload_protocol,
            payload: Vec::new(),
            request_id: 7,
            payload_encoding: PayloadEncoding::None as i32,
            deadline_unix_ms: 0,
            traceparent: String::new(),
            tracestate: String::new(),
        }
    }

    #[test]
    fn accepts_well_formed_envelope() {
        let frame = valid_frame(FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL);
        assert_eq!(
            validate_frame_envelope(&frame, FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL),
            Ok(())
        );
    }

    #[test]
    fn rejects_wrong_envelope_version_first() {
        let mut frame = valid_frame(FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL);
        frame.envelope_version = 2;
        // Also break later fields to prove version is checked first.
        frame.payload_protocol = HANDOFF_PAYLOAD_PROTOCOL;
        assert_eq!(
            validate_frame_envelope(&frame, FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL),
            Err(FrameValidationError::EnvelopeVersion { actual: 2 })
        );
    }

    #[test]
    fn rejects_unexpected_kind() {
        let frame = valid_frame(FrameKind::Event, CONTROL_PAYLOAD_PROTOCOL);
        assert_eq!(
            validate_frame_envelope(&frame, FrameKind::Response, CONTROL_PAYLOAD_PROTOCOL),
            Err(FrameValidationError::Kind {
                expected: FrameKind::Response,
                actual: FrameKind::Event as i32,
            })
        );
    }

    #[test]
    fn rejects_unknown_kind_value() {
        let mut frame = valid_frame(FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL);
        frame.kind = 99;
        assert_eq!(
            validate_frame_envelope(&frame, FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL),
            Err(FrameValidationError::Kind {
                expected: FrameKind::Request,
                actual: 99,
            })
        );
    }

    #[test]
    fn rejects_unexpected_payload_protocol() {
        let frame = valid_frame(FrameKind::Request, HANDOFF_PAYLOAD_PROTOCOL);
        assert_eq!(
            validate_frame_envelope(&frame, FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL),
            Err(FrameValidationError::PayloadProtocol {
                expected: CONTROL_PAYLOAD_PROTOCOL,
                actual: HANDOFF_PAYLOAD_PROTOCOL,
            })
        );
    }

    #[test]
    fn rejects_compressed_payload() {
        let mut frame = valid_frame(FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL);
        frame.payload_encoding = PayloadEncoding::Zstd as i32;
        assert_eq!(
            validate_frame_envelope(&frame, FrameKind::Request, CONTROL_PAYLOAD_PROTOCOL),
            Err(FrameValidationError::PayloadEncoding {
                actual: PayloadEncoding::Zstd as i32,
            })
        );
    }
}
