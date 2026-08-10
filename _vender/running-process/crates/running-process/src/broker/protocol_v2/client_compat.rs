//! v1-client compatibility surface under the v2 namespace
//! (slice 25-A of zccache#782).
//!
//! Re-exports the v1 broker-client + adopt types from [`super::super::client`]
//! and [`super::super::adopt`] under the v2 namespace so downstream consumers
//! can complete the literal "no `running_process::broker::{adopt,client}::*`
//! imports" milestone of their v1→v2 burn-down without breaking the
//! production broker connection path.
//!
//! ## Why a re-export, not a parallel client
//!
//! The "true" v2 broker client ([`super::super::client_v2`]) is already
//! published — what it lacks is a v2 broker SERVER to connect to. The
//! `running-process-broker-v2` binary is a scaffold (PRs #486–#489) without
//! an accept loop yet; consumers that need to actually adopt + handle
//! traffic still have to dial the v1 broker.
//!
//! Forcing consumers to keep `use running_process::broker::client::*`
//! imports during this window pollutes their dependency graph with a
//! "v1 surface" marker that lives forever in PR diffs and grep output.
//! The re-export under `protocol_v2::client_compat` is the honest
//! intermediate state: "consumer depends on the v2 namespace for its
//! broker types; the implementation under the namespace is v1 until
//! v2 broker is feature-complete."
//!
//! When [`super::super::client_v2::connect`] becomes production-ready
//! (accepting hello frames from a real v2 broker, threading adopt
//! through `BrokeredBackend`), this module's re-exports get swapped
//! for `client_v2::*` equivalents. The CONSUMER side doesn't change.
//!
//! ## Migration contract
//!
//! Replace:
//! ```rust,ignore
//! use running_process::broker::adopt::{AdoptError, AsyncBrokerSession, OwnedConnectRequest};
//! use running_process::broker::client::{BrokerClientError, BackendConnectionRoute, RefusalKind};
//! ```
//! with:
//! ```rust,ignore
//! use running_process::broker::protocol_v2::client_compat::{
//!     AdoptError, AsyncBrokerSession, OwnedConnectRequest,
//!     BrokerClientError, BackendConnectionRoute, RefusalKind,
//! };
//! ```
//!
//! Identical Rust API, identical wire behaviour, identical errors.

// Re-export every v1 adopt symbol zccache consumes. `AsyncBrokerSession`
// + `OwnedConnectRequest` are gated on `client-async` (#433 R3) — both
// upstream and downstream zccache enable that feature, but mirror the
// gate here so the re-export compiles with `--features client` alone.
pub use super::super::adopt::AdoptError;

#[cfg(feature = "client-async")]
pub use super::super::adopt::{
    AsyncBrokerSession, IntoBackendIoError, OwnedBackendIo, OwnedConnectRequest,
};

// Re-export every v1 client symbol zccache consumes.
pub use super::super::client::{BackendConnectionRoute, BrokerClientError, RefusalKind};

/// Classify a v2 broker error the way a v1 consumer classifies a refusal.
///
/// The first piece of the `client_compat` swap described above (#532
/// criterion 5). Consumers branch on [`RefusalKind`] today — zccache maps
/// `BrokerV2Error` in exactly one place — so the swap needs the v2 error to
/// answer the same question before anything else can move.
///
/// `None` means "not a refusal": a dial failure, a framing error, or an I/O
/// error is a different category, and flattening those into a `RefusalKind`
/// would tell a caller the broker said no when in fact it was never reached.
/// That distinction drives retry behaviour, so it is worth the `Option`.
///
/// The mapping itself is deliberately delegated to [`RefusalKind::from_code`]
/// rather than restated. A second copy of that match is a second thing to
/// keep in step, and the failure would be silent: a new `ErrorCode` would
/// classify one way through v1 and another through v2.
pub fn refusal_kind(error: &super::super::client_v2::BrokerV2Error) -> Option<RefusalKind> {
    match error {
        super::super::client_v2::BrokerV2Error::Refused { details, .. } => {
            Some(RefusalKind::from_code(details.code()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slice 25-A contract: every v1 broker-client + adopt symbol zccache
    /// imports is reachable through the v2 namespace at the same TypeId.
    /// A future upstream rename or fork catches here as a build break.
    fn refused_v2(
        code: crate::broker::protocol::ErrorCode,
    ) -> super::super::super::client_v2::BrokerV2Error {
        let mut refused = crate::broker::protocol::Refused {
            reason: "nope".into(),
            ..Default::default()
        };
        refused.set_code(code);
        super::super::super::client_v2::BrokerV2Error::Refused {
            reason: "nope".into(),
            retry_after_ms: 0,
            details: Box::new(refused),
        }
    }

    #[test]
    fn a_v2_refusal_classifies_the_same_as_a_v1_one() {
        use crate::broker::protocol::ErrorCode;
        // The property the swap depends on: a consumer branching on
        // `RefusalKind` sees the same answer whichever client produced it.
        // Checked across the whole enum rather than one variant, because a
        // mapping that is right for `ServiceUnknown` and wrong for
        // `RateLimited` still compiles and still looks tested.
        for code in [
            ErrorCode::ErrorVersionUnsupported,
            ErrorCode::ErrorVersionBlocked,
            ErrorCode::ErrorServiceUnknown,
            ErrorCode::ErrorRateLimited,
            ErrorCode::ErrorShuttingDown,
        ] {
            assert_eq!(
                refusal_kind(&refused_v2(code)),
                Some(RefusalKind::from_code(code)),
                "v2 refusal for {code:?} classified differently from v1"
            );
        }
    }

    #[test]
    fn an_unknown_code_stays_unknown_rather_than_becoming_a_named_refusal() {
        use crate::broker::protocol::ErrorCode;
        // A code this build does not know must arrive as `Other`, not as the
        // nearest named variant. Guessing here would tell a caller the broker
        // said something specific that it did not.
        let kind = refusal_kind(&refused_v2(ErrorCode::Unspecified));
        assert_eq!(kind, Some(RefusalKind::from_code(ErrorCode::Unspecified)));
        assert!(matches!(kind, Some(RefusalKind::Other(_))));
    }

    #[test]
    fn a_transport_failure_is_not_a_refusal() {
        // The distinction that drives retry behaviour: the broker saying no
        // is not the same as never reaching it. Flattening a dial or I/O
        // error into a `RefusalKind` would report a decision the broker never
        // made.
        let io = super::super::super::client_v2::BrokerV2Error::Io(std::io::Error::other("boom"));
        assert_eq!(refusal_kind(&io), None);
    }

    #[test]
    fn v1_client_adopt_types_are_aliased_under_v2_namespace() {
        use std::any::TypeId;

        // adopt: AdoptError always; AsyncBrokerSession + OwnedConnectRequest
        // gated on `client-async` (#433 R3).
        assert_eq!(
            TypeId::of::<super::super::super::adopt::AdoptError>(),
            TypeId::of::<AdoptError>(),
            "AdoptError aliased"
        );
        #[cfg(feature = "client-async")]
        {
            assert_eq!(
                TypeId::of::<super::super::super::adopt::AsyncBrokerSession>(),
                TypeId::of::<AsyncBrokerSession>(),
                "AsyncBrokerSession aliased"
            );
            assert_eq!(
                TypeId::of::<super::super::super::adopt::OwnedConnectRequest>(),
                TypeId::of::<OwnedConnectRequest>(),
                "OwnedConnectRequest aliased"
            );
        }

        // client: BackendConnectionRoute, BrokerClientError, RefusalKind.
        assert_eq!(
            TypeId::of::<super::super::super::client::BackendConnectionRoute>(),
            TypeId::of::<BackendConnectionRoute>(),
            "BackendConnectionRoute aliased"
        );
        assert_eq!(
            TypeId::of::<super::super::super::client::BrokerClientError>(),
            TypeId::of::<BrokerClientError>(),
            "BrokerClientError aliased"
        );
        assert_eq!(
            TypeId::of::<super::super::super::client::RefusalKind>(),
            TypeId::of::<RefusalKind>(),
            "RefusalKind aliased"
        );
    }
}
