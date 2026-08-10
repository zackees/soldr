//! Shared capability bitmap constants for the v1 Hello/Negotiated exchange.
//!
//! `Hello.client_capabilities` and `Negotiated.server_capabilities` carry a
//! `u64` bitmap. A capability is in effect only when both sides advertise it.
//! Bit assignments are FROZEN once a release ships them — never reuse or
//! renumber a bit (#228 frozen-forever commitments, #354 handoff tracker).

/// Capability bit: the peer can participate in broker-to-backend connection
/// handoff via platform handle passing (`DuplicateHandle` on Windows,
/// `SCM_RIGHTS` on Unix).
///
/// When negotiated, the broker issues a one-time pending handoff token in
/// `Negotiated.handle_passed_token`. Actual handle adoption is a later slice;
/// reconnecting to `Negotiated.backend_pipe` remains the correctness path.
pub const CAP_HANDLE_PASSING: u64 = 1 << 0;

/// Whether this build carries a platform handle-passing transport at all.
///
/// Currently `true` on both Windows (`DuplicateHandle`) and Unix
/// (`SCM_RIGHTS`), so this is effectively always `true`, but the check is
/// kept explicit so a future target without a transport degrades to the
/// reconnect path instead of advertising a capability it cannot honor.
pub const fn handoff_transport_available() -> bool {
    cfg!(any(windows, unix))
}
