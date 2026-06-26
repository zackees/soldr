//! soldr-daemon — long-lived process that owns target/ tracking, build
//! event correlation (Phase 2), and linked zccache shutdown (Phase 3).
//!
//! Phase 1 scope (this module): IPC server + sync client + PID-based
//! lifecycle, fronting the existing `TargetRegistry` redb writes. The
//! wrapper hot path tries the daemon first via a fire-and-forget
//! socket/pipe write with a 50 ms timeout, then falls back to writing
//! the redb file directly so a missing or unhealthy daemon never blocks
//! a build.
//!
//! The daemon binary is `soldr-daemon` (declared in
//! `crates/soldr-cli/Cargo.toml`); its entry point at
//! `src/bin/soldr_daemon.rs` delegates straight into `server::run()`.
//!
//! `dead_code` is allowed module-wide because the bin and lib compile
//! `daemon::*` independently — symbols used only by the integration
//! tests (lib path) or only by the daemon entry point (bin path) would
//! otherwise be flagged in the other build.

#![allow(dead_code, unused_imports)]

pub mod backend_handle_adoption;
pub mod broker_discovery;
pub mod client;
/// Phase 2 of issue #977 — `CompileBackend::{Wrapped, Embedded}` enum
/// held on `server::State`. Unconditional (even without `--features
/// embedded`) so the daemon always has the abstraction in scope; the
/// `Embedded` variant itself is gated on the feature.
pub mod compile_backend;
pub mod db;
pub mod ipc;
pub mod lifecycle;
pub mod protocol;
pub mod server;
pub mod service_definition;
pub mod wire;
pub mod zccache_link;
