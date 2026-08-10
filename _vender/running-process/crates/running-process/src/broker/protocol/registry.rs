//! Single authoritative registry of v1 broker wire-protocol constants
//! (#375).
//!
//! Every payload-protocol ID multiplexed over the frozen v1 `Frame`
//! envelope, plus the negotiated protocol version, is defined HERE and
//! only here. Subsystem modules `pub use` these constants (the old
//! public paths remain valid re-export shims) so the public API surface
//! is unchanged while the values have exactly one definition site.
//!
//! These values are part of the FROZEN-FOREVER v1 wire contract — see
//! `proto/broker_v1_envelope.proto` and #228. Never change an existing
//! value; only append new IDs (and keep them pairwise distinct — the
//! unit test below enforces this).
//!
//! # Payload-protocol ID registry
//!
//! | ID       | Constant                                   | Purpose                                        | Payload proto message            |
//! |----------|--------------------------------------------|------------------------------------------------|----------------------------------|
//! | `0x0000` | [`CONTROL_PAYLOAD_PROTOCOL`]               | Control plane: client Hello / broker HelloReply | `Hello` / `HelloReply`           |
//! | `0xAD01` | [`ADMIN_PAYLOAD_PROTOCOL`]                 | Admin verbs over the broker control socket      | `AdminRequest` / `AdminReply`    |
//! | `0xB232` | [`BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL`]  | `BackendHandle` endpoint identity probes        | raw nonce / nonce + `DaemonProcess` |
//! | `0xD0FF` | [`HANDOFF_PAYLOAD_PROTOCOL`]               | Connection handoff offer/ACK and client relay   | `HandoffOffer` / `HandoffAck`    |
//!
//! # Consumer payload-protocol IDs (#412)
//!
//! Consumer daemons that multiplex their own request/response payloads
//! over the v1 `Frame` envelope (the "opaque Frame lane" pattern — see
//! `docs/INTEGRATE.md`) pick an ID from the registered-consumer range
//! and record it in the table below via a running-process PR. IDs are
//! first-come-first-served and frozen once registered.
//!
//! | Range               | Use                                                     |
//! |---------------------|---------------------------------------------------------|
//! | `0x0000`–`0x6FFF`   | Reserved for first-party running-process subsystems     |
//! | `0x7000`–`0x7EFF`   | Registered consumer IDs (table below)                   |
//! | `0x7F00`–`0xEFFF`   | Reserved for future expansion — do not use              |
//! | `0xF000`–`0xFFFF`   | Private use — never registered, never collision-checked |
//!
//! (`0xAD01`, `0xB232`, and `0xD0FF` predate the range split and are
//! grandfathered first-party IDs; [`is_first_party`] knows about them.)
//!
//! | ID       | Constant                     | Consumer                                                         |
//! |----------|------------------------------|------------------------------------------------------------------|
//! | `0x7A63` | [`ZCCACHE_PAYLOAD_PROTOCOL`] | zccache (`"zc"` in ASCII; zccache FrameV1 request/response lane) |
//! | `0x7C4C` | [`CLUD_PAYLOAD_PROTOCOL`]    | clud (`0x7C4C` = `"|L"`; clud daemon Frame v1 request/response lane) |
//! | `0x7EB1` | [`FBUILD_PAYLOAD_PROTOCOL`]  | fbuild (FastLED/fbuild daemon Frame v1 request/response lane) |
//!
//! Use [`crate::register_payload_protocol!`] in consumer crates to pin
//! the chosen ID with compile-time range and collision checks.

/// Negotiated v1 broker protocol version.
///
/// This is the value carried in `Frame.envelope_version` (a u32 protobuf
/// field) and in the `Hello`/`Refused`/`Negotiated` protocol-range fields
/// (`client_min_protocol`, `daemon_max_protocol`, `negotiated_protocol`,
/// ...).
///
/// NOT to be confused with [`crate::broker::FRAMING_VERSION_V1`]: that is
/// the single raw `u8` framing byte written before every length-prefixed
/// frame body on the wire. Both happen to be `1` in v1, but they are
/// conceptually distinct version axes (outer framing layout vs negotiated
/// protocol schema) and MUST stay separate named constants.
pub const PROTOCOL_VERSION: u32 = 1;

/// Outer framing byte for every v1 broker connection (re-exported here so
/// the registry names both version axes side by side).
///
/// Distinct from [`PROTOCOL_VERSION`]: this `u8` prefixes the raw wire
/// layout `[u8 framing_version][u32 LE body_length][prost body]`, while
/// `PROTOCOL_VERSION` is the negotiated protocol schema version carried
/// inside the decoded `Frame`. Both are `1` in v1 by coincidence — do not
/// collapse them. Canonical definition: [`crate::broker::FRAMING_VERSION_V1`].
pub use crate::broker::FRAMING_VERSION_V1;

/// Payload protocol for v1 control-plane frames (Hello / HelloReply).
pub const CONTROL_PAYLOAD_PROTOCOL: u32 = 0x00;

/// Payload protocol value for v1 admin request/reply frames.
pub const ADMIN_PAYLOAD_PROTOCOL: u32 = 0xAD01;

/// Payload protocol reserved for `BackendHandle` endpoint identity probes.
pub const BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL: u32 = 0xB232;

/// Payload protocol reserved for broker↔backend handoff offer/ACK frames
/// and the broker→client handoff-ready relay.
pub const HANDOFF_PAYLOAD_PROTOCOL: u32 = 0xD0FF;

/// Inclusive lower bound of the registered-consumer payload-protocol range.
pub const CONSUMER_PAYLOAD_PROTOCOL_MIN: u32 = 0x7000;

/// Inclusive upper bound of the registered-consumer payload-protocol range.
pub const CONSUMER_PAYLOAD_PROTOCOL_MAX: u32 = 0x7EFF;

/// Inclusive lower bound of the private-use payload-protocol range.
///
/// Private-use IDs are never registered here and never collision-checked
/// against other consumers — suitable for tests and closed deployments only.
pub const PRIVATE_USE_PAYLOAD_PROTOCOL_MIN: u32 = 0xF000;

/// Inclusive upper bound of the private-use payload-protocol range.
pub const PRIVATE_USE_PAYLOAD_PROTOCOL_MAX: u32 = 0xFFFF;

/// Registered consumer ID: zccache's opaque FrameV1 request/response lane
/// (`0x7A63` = ASCII `"zc"`). zccache pins this value on its side with
/// [`crate::register_payload_protocol!`]-style compile-time asserts; the
/// authoritative registration lives here.
pub const ZCCACHE_PAYLOAD_PROTOCOL: u32 = 0x7A63;

/// Registered consumer ID: clud's opaque Frame v1 request/response lane
/// (zackees/clud daemon control plane; zackees/running-process#385).
/// clud pins this value on its side with
/// [`crate::register_payload_protocol!`]-style compile-time asserts; the
/// authoritative registration lives here.
pub const CLUD_PAYLOAD_PROTOCOL: u32 = 0x7C4C;

/// Registered consumer ID: fbuild's opaque Frame v1 request/response lane
/// (FastLED/fbuild daemon control plane; zackees/running-process#437). fbuild
/// pins this value on its side with [`crate::register_payload_protocol!`]; the
/// authoritative registration lives here.
pub const FBUILD_PAYLOAD_PROTOCOL: u32 = 0x7EB1;

/// All first-party payload-protocol IDs, in registry-table order.
///
/// Used by [`is_first_party`] and the consumer-side collision checks
/// emitted by [`crate::register_payload_protocol!`].
pub const FIRST_PARTY_PAYLOAD_PROTOCOLS: [u32; 4] = [
    CONTROL_PAYLOAD_PROTOCOL,
    ADMIN_PAYLOAD_PROTOCOL,
    BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
    HANDOFF_PAYLOAD_PROTOCOL,
];

/// True when `id` is a first-party running-process payload-protocol ID.
///
/// Consumer payload protocols must never collide with these; the
/// [`crate::register_payload_protocol!`] macro enforces that at compile
/// time.
pub const fn is_first_party(id: u32) -> bool {
    let mut index = 0;
    while index < FIRST_PARTY_PAYLOAD_PROTOCOLS.len() {
        if FIRST_PARTY_PAYLOAD_PROTOCOLS[index] == id {
            return true;
        }
        index += 1;
    }
    false
}

/// True when `id` falls inside the registered-consumer range
/// (`0x7000..=0x7EFF`).
pub const fn is_registered_consumer_id(id: u32) -> bool {
    id >= CONSUMER_PAYLOAD_PROTOCOL_MIN && id <= CONSUMER_PAYLOAD_PROTOCOL_MAX
}

/// True when `id` falls inside the private-use range (`0xF000..=0xFFFF`).
pub const fn is_private_use_id(id: u32) -> bool {
    id >= PRIVATE_USE_PAYLOAD_PROTOCOL_MIN && id <= PRIVATE_USE_PAYLOAD_PROTOCOL_MAX
}

/// Define a consumer payload-protocol constant with compile-time checks.
///
/// Expands to a `pub const` plus const asserts that the value:
///
/// 1. does not collide with any first-party running-process payload
///    protocol ([`registry::is_first_party`](is_first_party)), and
/// 2. lies inside the registered-consumer range (`0x7000..=0x7EFF`) or
///    the private-use range (`0xF000..=0xFFFF`).
///
/// ```
/// running_process::register_payload_protocol! {
///     /// My daemon's opaque Frame lane.
///     pub const MY_PAYLOAD_PROTOCOL: u32 = 0x7A63;
/// }
/// assert_eq!(MY_PAYLOAD_PROTOCOL, 0x7A63);
/// ```
#[macro_export]
macro_rules! register_payload_protocol {
    ($(#[$meta:meta])* $vis:vis const $name:ident: u32 = $value:expr;) => {
        $(#[$meta])*
        $vis const $name: u32 = $value;

        const _: () = {
            assert!(
                !$crate::broker::protocol::registry::is_first_party($name),
                concat!(
                    stringify!($name),
                    " collides with a first-party running-process payload protocol",
                ),
            );
            assert!(
                $crate::broker::protocol::registry::is_registered_consumer_id($name)
                    || $crate::broker::protocol::registry::is_private_use_id($name),
                concat!(
                    stringify!($name),
                    " must lie in the registered-consumer range (0x7000..=0x7EFF) \
                     or the private-use range (0xF000..=0xFFFF)",
                ),
            );
        };
    };
}

#[cfg(test)]
mod tests {
    use super::{
        ADMIN_PAYLOAD_PROTOCOL, BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, CONTROL_PAYLOAD_PROTOCOL,
        HANDOFF_PAYLOAD_PROTOCOL, PROTOCOL_VERSION,
    };

    /// Every registered payload-protocol ID must be pairwise distinct —
    /// they multiplex independent subsystems over one frame envelope.
    #[test]
    fn payload_protocol_ids_are_pairwise_distinct() {
        let registered: [(u32, &str); 4] = [
            (CONTROL_PAYLOAD_PROTOCOL, "CONTROL_PAYLOAD_PROTOCOL"),
            (ADMIN_PAYLOAD_PROTOCOL, "ADMIN_PAYLOAD_PROTOCOL"),
            (
                BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL,
                "BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL",
            ),
            (HANDOFF_PAYLOAD_PROTOCOL, "HANDOFF_PAYLOAD_PROTOCOL"),
        ];
        for (left_index, (left_id, left_name)) in registered.iter().enumerate() {
            for (right_id, right_name) in &registered[left_index + 1..] {
                assert_ne!(
                    left_id, right_id,
                    "{left_name} and {right_name} share payload-protocol id {left_id:#06X}"
                );
            }
        }
    }

    /// Frozen v1 wire values — changing any of these breaks the v1 contract.
    #[test]
    fn frozen_v1_wire_values() {
        assert_eq!(PROTOCOL_VERSION, 1);
        assert_eq!(CONTROL_PAYLOAD_PROTOCOL, 0x00);
        assert_eq!(ADMIN_PAYLOAD_PROTOCOL, 0xAD01);
        assert_eq!(BACKEND_HANDLE_PROBE_PAYLOAD_PROTOCOL, 0xB232);
        assert_eq!(HANDOFF_PAYLOAD_PROTOCOL, 0xD0FF);
        assert_eq!(u32::from(crate::broker::FRAMING_VERSION_V1), 1);
    }

    /// Frozen consumer registrations and range boundaries (#412).
    #[test]
    fn frozen_consumer_registry_values() {
        use super::{
            is_first_party, is_private_use_id, is_registered_consumer_id, CLUD_PAYLOAD_PROTOCOL,
            CONSUMER_PAYLOAD_PROTOCOL_MAX, CONSUMER_PAYLOAD_PROTOCOL_MIN, FBUILD_PAYLOAD_PROTOCOL,
            PRIVATE_USE_PAYLOAD_PROTOCOL_MAX, PRIVATE_USE_PAYLOAD_PROTOCOL_MIN,
            ZCCACHE_PAYLOAD_PROTOCOL,
        };

        assert_eq!(CONSUMER_PAYLOAD_PROTOCOL_MIN, 0x7000);
        assert_eq!(CONSUMER_PAYLOAD_PROTOCOL_MAX, 0x7EFF);
        assert_eq!(PRIVATE_USE_PAYLOAD_PROTOCOL_MIN, 0xF000);
        assert_eq!(PRIVATE_USE_PAYLOAD_PROTOCOL_MAX, 0xFFFF);
        assert_eq!(ZCCACHE_PAYLOAD_PROTOCOL, 0x7A63);
        assert_eq!(CLUD_PAYLOAD_PROTOCOL, 0x7C4C);
        assert_eq!(FBUILD_PAYLOAD_PROTOCOL, 0x7EB1);
        assert_ne!(CLUD_PAYLOAD_PROTOCOL, ZCCACHE_PAYLOAD_PROTOCOL);
        assert_ne!(FBUILD_PAYLOAD_PROTOCOL, ZCCACHE_PAYLOAD_PROTOCOL);
        assert_ne!(FBUILD_PAYLOAD_PROTOCOL, CLUD_PAYLOAD_PROTOCOL);

        assert!(is_registered_consumer_id(ZCCACHE_PAYLOAD_PROTOCOL));
        assert!(!is_first_party(ZCCACHE_PAYLOAD_PROTOCOL));
        assert!(!is_private_use_id(ZCCACHE_PAYLOAD_PROTOCOL));

        assert!(is_registered_consumer_id(CLUD_PAYLOAD_PROTOCOL));
        assert!(!is_first_party(CLUD_PAYLOAD_PROTOCOL));
        assert!(!is_private_use_id(CLUD_PAYLOAD_PROTOCOL));

        assert!(is_registered_consumer_id(FBUILD_PAYLOAD_PROTOCOL));
        assert!(!is_first_party(FBUILD_PAYLOAD_PROTOCOL));
        assert!(!is_private_use_id(FBUILD_PAYLOAD_PROTOCOL));

        // First-party IDs must never drift into the consumer range.
        for id in super::FIRST_PARTY_PAYLOAD_PROTOCOLS {
            assert!(is_first_party(id));
            assert!(!is_registered_consumer_id(id));
            assert!(!is_private_use_id(id));
        }
    }

    // The macro must compile for both allowed ranges.
    crate::register_payload_protocol! {
        /// Registered-consumer-range example (compile-time check).
        const MACRO_CONSUMER_RANGE_EXAMPLE: u32 = 0x7001;
    }
    crate::register_payload_protocol! {
        /// Private-use-range example (compile-time check).
        const MACRO_PRIVATE_RANGE_EXAMPLE: u32 = 0xF00D;
    }

    #[test]
    fn register_macro_defines_usable_constants() {
        assert_eq!(MACRO_CONSUMER_RANGE_EXAMPLE, 0x7001);
        assert_eq!(MACRO_PRIVATE_RANGE_EXAMPLE, 0xF00D);
    }
}
