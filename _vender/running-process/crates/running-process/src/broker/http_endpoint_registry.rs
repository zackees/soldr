//! Per-backend HTTP endpoint registry for the v2 broker (slice 5 of #488).
//!
//! Stores `BackendId → Option<u16>` so the v2 broker knows which port
//! each registered backend's HTTP server (if any) is listening on.
//! Plumbed by the broker↔daemon control plane: when a daemon emits a
//! `BackendHttpReady` frame, the broker decodes it via
//! `decode_and_register` and stores the port against the backend id
//! it tracks.
//!
//! No HTTP server lives here. That arrives in slice 7. This slice is
//! purely the registry + frame plumbing — the state every subsequent
//! HTTP-related slice needs to read from.

use std::collections::HashMap;
use std::sync::Mutex;

use prost::Message;

use crate::broker::protocol_v2::BackendHttpReady;

/// Identifier for a backend the broker is tracking. The v2 broker uses
/// a `String` for transparency at this slice; later slices may swap to
/// a typed wrapper as the registry grows companion fields.
pub type BackendId = String;

/// Thread-safe map of `BackendId → Option<u16>` per the design in #483 §2.
///
/// `None` means the backend exists but has not yet reported a port
/// (its HTTP server hasn't bound). `Some(port)` means it has, and the
/// aggregator iframe can resolve a URL.
#[derive(Debug, Default)]
pub struct HttpEndpointRegistry {
    inner: Mutex<HashMap<BackendId, Option<u16>>>,
}

impl HttpEndpointRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark `backend_id` as tracked with no port yet.
    pub fn track(&self, backend_id: BackendId) {
        let mut map = self.inner.lock().expect("registry mutex poisoned");
        map.entry(backend_id).or_insert(None);
    }

    /// Record that `backend_id`'s HTTP server has bound `port`.
    ///
    /// Inserts the backend if it wasn't already tracked. Returns the
    /// previous port for that backend if any.
    pub fn register_backend_http_endpoint(&self, backend_id: BackendId, port: u16) -> Option<u16> {
        let mut map = self.inner.lock().expect("registry mutex poisoned");
        map.insert(backend_id, Some(port)).flatten()
    }

    /// Look up the port for `backend_id`, if any.
    ///
    /// Returns `None` both when the backend is untracked AND when it
    /// is tracked but hasn't reported a port yet — the aggregator
    /// uses the broader `state()` API below when it needs the distinction.
    pub fn lookup(&self, backend_id: &str) -> Option<u16> {
        let map = self.inner.lock().expect("registry mutex poisoned");
        map.get(backend_id).copied().flatten()
    }

    /// Get the current state for `backend_id`.
    ///
    /// `Some(Some(port))` = registered + bound. `Some(None)` = tracked
    /// but starting. `None` = untracked.
    pub fn state(&self, backend_id: &str) -> Option<Option<u16>> {
        let map = self.inner.lock().expect("registry mutex poisoned");
        map.get(backend_id).copied()
    }

    /// Snapshot of all currently-tracked backends and their state.
    /// Used by the aggregator selector and by tests.
    pub fn snapshot(&self) -> Vec<(BackendId, Option<u16>)> {
        let map = self.inner.lock().expect("registry mutex poisoned");
        map.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }
}

/// Errors raised by [`decode_and_register`].
#[derive(Debug, thiserror::Error)]
pub enum BackendHttpReadyError {
    /// The incoming bytes did not decode as a `BackendHttpReady`.
    #[error("decode BackendHttpReady: {0}")]
    Decode(#[from] prost::DecodeError),

    /// The decoded `port` did not fit in a `u16` (i.e. > 65535).
    #[error("BackendHttpReady.port = {0} is out of u16 range")]
    PortOutOfRange(u32),
}

/// Decode a `BackendHttpReady` frame body and register the port against
/// `backend_id` in `registry`.
///
/// `frame_body` is the prost-encoded message bytes (the body inside the
/// envelope produced by `protocol::write_frame`). Returns the registered
/// port on success.
pub fn decode_and_register(
    registry: &HttpEndpointRegistry,
    backend_id: BackendId,
    frame_body: &[u8],
) -> Result<u16, BackendHttpReadyError> {
    let ready = BackendHttpReady::decode(frame_body)?;
    let port: u16 = ready
        .port
        .try_into()
        .map_err(|_| BackendHttpReadyError::PortOutOfRange(ready.port))?;
    registry.register_backend_http_endpoint(backend_id, port);
    Ok(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_has_no_state_for_unknown_backend() {
        let reg = HttpEndpointRegistry::new();
        assert!(reg.state("zccache").is_none());
        assert!(reg.lookup("zccache").is_none());
    }

    #[test]
    fn track_then_lookup_returns_none_for_pending_port() {
        let reg = HttpEndpointRegistry::new();
        reg.track("zccache".to_string());
        // Tracked but no port yet — lookup still returns None.
        assert!(reg.lookup("zccache").is_none());
        // But state() distinguishes tracked-no-port from untracked.
        assert_eq!(reg.state("zccache"), Some(None));
    }

    #[test]
    fn register_endpoint_makes_port_available() {
        let reg = HttpEndpointRegistry::new();
        reg.track("zccache".to_string());
        let prev = reg.register_backend_http_endpoint("zccache".to_string(), 8765);
        assert_eq!(prev, None);
        assert_eq!(reg.lookup("zccache"), Some(8765));
        assert_eq!(reg.state("zccache"), Some(Some(8765)));
    }

    #[test]
    fn register_endpoint_updates_existing_port_and_returns_previous() {
        let reg = HttpEndpointRegistry::new();
        reg.register_backend_http_endpoint("fbuild".to_string(), 8001);
        let prev = reg.register_backend_http_endpoint("fbuild".to_string(), 8002);
        assert_eq!(prev, Some(8001));
        assert_eq!(reg.lookup("fbuild"), Some(8002));
    }

    #[test]
    fn snapshot_reflects_all_tracked_backends() {
        let reg = HttpEndpointRegistry::new();
        reg.track("zccache".to_string());
        reg.register_backend_http_endpoint("fbuild".to_string(), 8002);

        let mut snap = reg.snapshot();
        snap.sort();
        assert_eq!(
            snap,
            vec![
                ("fbuild".to_string(), Some(8002)),
                ("zccache".to_string(), None)
            ]
        );
    }

    #[test]
    fn decode_and_register_happy_path() {
        let reg = HttpEndpointRegistry::new();
        let msg = BackendHttpReady { port: 49_152 };
        let mut body = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut body).expect("encode BackendHttpReady");

        let port = decode_and_register(&reg, "zccache".to_string(), &body)
            .expect("decode_and_register succeeds");
        assert_eq!(port, 49_152);
        assert_eq!(reg.lookup("zccache"), Some(49_152));
    }

    #[test]
    fn decode_and_register_rejects_oversized_port() {
        let reg = HttpEndpointRegistry::new();
        // Encode a BackendHttpReady carrying a port that overflows u16.
        let msg = BackendHttpReady { port: 70_000 };
        let mut body = Vec::with_capacity(msg.encoded_len());
        msg.encode(&mut body).expect("encode BackendHttpReady");

        let err = decode_and_register(&reg, "zccache".to_string(), &body)
            .expect_err("port=70000 should be rejected");
        match err {
            BackendHttpReadyError::PortOutOfRange(70_000) => {}
            other => panic!("expected PortOutOfRange(70000), got: {other:?}"),
        }
        // Registry untouched.
        assert!(reg.lookup("zccache").is_none());
    }

    #[test]
    fn decode_and_register_rejects_malformed_frame() {
        let reg = HttpEndpointRegistry::new();
        // 0xFF is not a valid wire-type byte at start of a proto message.
        let err = decode_and_register(&reg, "zccache".to_string(), &[0xFF; 8])
            .expect_err("malformed frame should be rejected");
        match err {
            BackendHttpReadyError::Decode(_) => {}
            other => panic!("expected Decode, got: {other:?}"),
        }
        assert!(reg.lookup("zccache").is_none());
    }
}
