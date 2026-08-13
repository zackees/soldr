//! Host-neutral IPC facade.
//!
//! Owns endpoint representation and OS-safe name/path derivation, Unix
//! socket versus Windows named-pipe bind/connect/accept, owner-only
//! permissions and ACLs, transport deadlines, peer identity, stream and
//! descriptor/handle handoff, and endpoint existence and retirement.
//! Callers retain framing, request/reply semantics, broker routing,
//! lifecycle state, and product retry policy.
//!
//! Filled by the #2493 migration; the concrete implementations live in the
//! `platform_win` / `platform_linux` / `platform_macos` trees and are
//! re-exported here through [`crate::platform_imp`].
