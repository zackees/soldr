//! v2 backend-handle namespace (slice 23-B of zccache#782).
//!
//! Re-exports the cross-version-stable broker-side process identity
//! types from [`super::super::backend_handle`] under the v2 namespace
//! so downstream consumers can complete their v1→v2 import migration
//! mechanically.
//!
//! ## Why a re-export, not a parallel implementation
//!
//! Per the [broker-v2 coexistence design](https://github.com/zackees/running-process/issues/470),
//! a small set of types is intentionally shared across both broker
//! generations because they describe identity-level facts (PID,
//! executable SHA-256, boot ID, IPC endpoint, started-at timestamp)
//! that v2 does not need to redefine:
//!
//! - `DaemonProcess` — the (pid, exe_path, exe_sha256, ipc_endpoint,
//!   started_at_unix_ms, boot_id, idle_timeout_secs) tuple a broker
//!   uses to probe daemon liveness. v2 inherits these field semantics
//!   verbatim from v1; only the surrounding broker protocol (Hello,
//!   adopt, control RPCs) changes.
//! - `BackendHandle` — the verified-identity wrapper a broker returns
//!   from a successful probe. Same shape across v1 and v2.
//! - `Endpoint` — the `{scheme, path}` pair for a local IPC endpoint.
//!   Independent of broker version.
//! - `IdentityError` — the error taxonomy for identity construction.
//!   Cross-version-stable error variants.
//!
//! The v2 broker probe RPC itself uses these types unchanged. Future
//! v2-only identity fields (e.g. a v2 attestation nonce) would land
//! as separate `protocol_v2::backend_handle_v2_extensions` additions
//! rather than a fork of the base type.
//!
//! ## Migration contract for consumers
//!
//! Downstream consumers (zccache et al.) replace
//! `running_process::broker::backend_handle::*` with
//! `running_process::broker::protocol_v2::backend_handle::*`. The
//! Rust API surface is identical — `current_process`,
//! `probe_with_service`, `probe_with_service_async`, JSON serde
//! round-trip — so the migration is a `use`-statement swap.
//!
//! Once v1 retires entirely (zccache#782 slice 25 + later), the v1
//! `broker::backend_handle` module can become a re-export of THIS
//! module instead of the other way around. The directionality is
//! reversed but the bytes on the wire / the JSON on disk are
//! unchanged.

pub use super::super::backend_handle::{BackendHandle, BackendHandleError, Connection};

pub use super::super::backend_lifecycle::identity::{DaemonProcess, IdentityError};

pub use super::super::protocol::Endpoint;

/// Magic frame payload-protocol marker the `try_serve_backend_handle_probe`
/// reader uses to detect a broker probe vs a zccache message. Identical
/// to v1's `broker::backend_lifecycle::probe::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL`
/// — the constant is part of the cross-version-stable probe wire shape.
pub const BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL: u32 =
    super::super::backend_lifecycle::probe::BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL;

#[cfg(test)]
mod tests {
    use super::*;

    /// Slice 23-B: v1 and v2 share the underlying type identity for
    /// the broker-side process identity types. The v2 namespace is a
    /// pure re-export per the coexistence design (#470). Pin that
    /// the type identity is preserved so a future refactor that
    /// accidentally forks the types catches here.
    #[test]
    fn v1_and_v2_backend_handle_types_are_the_same() {
        use std::any::TypeId;

        let v1_dp = TypeId::of::<super::super::super::backend_handle::DaemonProcess>();
        let v2_dp = TypeId::of::<DaemonProcess>();
        assert_eq!(
            v1_dp, v2_dp,
            "v2::backend_handle::DaemonProcess must alias v1's during coexistence"
        );

        let v1_bh = TypeId::of::<super::super::super::backend_handle::BackendHandle>();
        let v2_bh = TypeId::of::<BackendHandle>();
        assert_eq!(v1_bh, v2_bh, "BackendHandle aliased");

        let v1_ep = TypeId::of::<super::super::super::protocol::Endpoint>();
        let v2_ep = TypeId::of::<Endpoint>();
        assert_eq!(v1_ep, v2_ep, "Endpoint aliased");

        let v1_err =
            TypeId::of::<super::super::super::backend_lifecycle::identity::IdentityError>();
        let v2_err = TypeId::of::<IdentityError>();
        assert_eq!(v1_err, v2_err, "IdentityError aliased");

        let v1_bhe = TypeId::of::<super::super::super::backend_handle::BackendHandleError>();
        let v2_bhe = TypeId::of::<BackendHandleError>();
        assert_eq!(v1_bhe, v2_bhe, "BackendHandleError aliased");
    }

    /// Slice 23-B: the `Endpoint` re-export has the v1 proto field
    /// shape (`namespace_id`, `path`). Pin the field set from the
    /// consumer side so a future schema change to v1's Endpoint
    /// surfaces immediately rather than at the first runtime decode.
    #[test]
    fn endpoint_has_namespace_id_and_path_fields() {
        let endpoint = Endpoint {
            namespace_id: "ns-7".to_owned(),
            path: "/tmp/example".to_owned(),
        };
        assert_eq!(endpoint.namespace_id, "ns-7");
        assert_eq!(endpoint.path, "/tmp/example");
    }
}
