//! `GetBrokerHttpEndpoint` RPC dispatch (slice 6 of #488).
//!
//! The CLI calls `GetBrokerHttpEndpoint` over the v2 broker control
//! channel to discover the broker's HTTP endpoint (per #483 §4 — the
//! single discovery surface). This module implements the broker side
//! of that RPC: given the broker's currently-resolved HTTP port + its
//! own pid, build a `GetBrokerHttpEndpointResponse` and serialize it.
//!
//! The real plumbing (read incoming frame → dispatch on payload type →
//! write response frame) lives in the broker's connection loop, which
//! is filled in by later slices. This slice exposes the typed
//! request/response handler so subsequent slices have a pinned API to
//! call.

use prost::Message;

use crate::broker::protocol_v2::{GetBrokerHttpEndpointRequest, GetBrokerHttpEndpointResponse};

/// In-broker resolved HTTP endpoint state (set at boot per #483 §3 via
/// `BrokerHttpPort::resolve(config, env)`).
#[derive(Debug, Clone, Copy)]
pub struct BrokerHttpEndpoint {
    /// The port the broker's own HTTP server bound. Slice 7 actually
    /// binds it; before then the broker can stub this to its
    /// configured-static port for early consumer testing.
    pub port: u16,
    /// The broker's process id. Used by consumers to disambiguate a
    /// fresh response from a stale one mid-restart (#483 §4 rationale).
    pub pid: u32,
}

impl BrokerHttpEndpoint {
    /// Build a `GetBrokerHttpEndpointResponse` carrying this endpoint.
    pub fn to_response(self) -> GetBrokerHttpEndpointResponse {
        GetBrokerHttpEndpointResponse {
            port: self.port as u32,
            pid: self.pid,
        }
    }
}

/// Errors from [`decode_request_and_dispatch`].
#[derive(Debug, thiserror::Error)]
pub enum GetHttpEndpointError {
    /// The incoming frame body did not decode as `GetBrokerHttpEndpointRequest`.
    #[error("decode GetBrokerHttpEndpointRequest: {0}")]
    Decode(#[from] prost::DecodeError),

    /// Encoding the response failed.
    #[error("encode GetBrokerHttpEndpointResponse: {0}")]
    Encode(#[from] prost::EncodeError),
}

/// Decode an incoming `GetBrokerHttpEndpointRequest` frame body and
/// produce a serialized `GetBrokerHttpEndpointResponse` body the
/// connection loop can write back via `protocol::write_frame`.
///
/// The request currently has no fields (`GetBrokerHttpEndpointRequest`
/// is an empty marker per #483 §4) — decoding is purely validation
/// that the peer sent a structurally well-formed proto message of the
/// expected type.
pub fn decode_request_and_dispatch(
    request_body: &[u8],
    endpoint: BrokerHttpEndpoint,
) -> Result<Vec<u8>, GetHttpEndpointError> {
    let _request = GetBrokerHttpEndpointRequest::decode(request_body)?;
    let response = endpoint.to_response();
    let mut body = Vec::with_capacity(response.encoded_len());
    response.encode(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_response_carries_port_and_pid() {
        let resp = BrokerHttpEndpoint {
            port: 8765,
            pid: 12_345,
        }
        .to_response();
        assert_eq!(resp.port, 8765);
        assert_eq!(resp.pid, 12_345);
    }

    #[test]
    fn dispatch_round_trip_with_empty_request() {
        let req = GetBrokerHttpEndpointRequest::default();
        let mut body = Vec::with_capacity(req.encoded_len());
        req.encode(&mut body).expect("encode request");

        let resp_body = decode_request_and_dispatch(
            &body,
            BrokerHttpEndpoint {
                port: 4242,
                pid: 99_999,
            },
        )
        .expect("dispatch succeeds");

        let resp =
            GetBrokerHttpEndpointResponse::decode(resp_body.as_slice()).expect("decode response");
        assert_eq!(resp.port, 4242);
        assert_eq!(resp.pid, 99_999);
    }

    #[test]
    fn dispatch_rejects_malformed_request_body() {
        let err = decode_request_and_dispatch(
            &[0xFF; 4],
            BrokerHttpEndpoint {
                port: 4242,
                pid: 99_999,
            },
        )
        .expect_err("malformed request body should be rejected");
        match err {
            GetHttpEndpointError::Decode(_) => {}
            other => panic!("expected Decode error, got: {other:?}"),
        }
    }
}
