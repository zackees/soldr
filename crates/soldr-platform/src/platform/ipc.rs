//! Host-neutral IPC facade.
//!
//! Owns endpoint representation and OS-safe name/path derivation, Unix
//! socket versus Windows named-pipe bind/connect/accept, owner-only
//! permissions and ACLs, transport deadlines, peer identity, stream and
//! descriptor/handle handoff, and endpoint existence and retirement.
//! Callers retain framing, request/reply semantics, broker routing,
//! lifecycle state, and product retry policy.

pub mod connect;
pub mod endpoint;
pub mod handoff;
pub mod listener;
pub mod peer;
