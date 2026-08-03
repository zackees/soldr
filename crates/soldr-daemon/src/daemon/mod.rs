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
/// soldr#2224 — the three IPC handlers that touch `state.redb`, split
/// out of the oversized `server.rs`.
pub mod build_session_ops;
pub mod client;
/// soldr#1857 — always-on JSONL record of compiles the daemon ran but
/// could not hand back to the wrapper. The artifact that distinguishes
/// "rustc rejected your code" from "soldr lost a finished compile".
pub mod compile_delivery;
/// Per-compile JSONL phase trace, gated by `SOLDR_DAEMON_TRACE`.
/// Diagnostic-only — see `compile_trace.rs` for format. Wired in by
/// soldr#981 to identify the per-compile dispatch bottleneck that
/// the zccache#939 buffer-elimination plan failed to find.
pub mod compile_trace;
pub mod db;
/// soldr#2224 — `spawn_blocking` wrappers so a contended `state.redb`
/// open never parks a tokio worker thread.
pub mod db_async;
/// soldr#1857 — the compile/disconnect race and the durable record of
/// what a lost connection cost. Split out of `server.rs`.
pub mod disconnect;
/// L4 (issue soldr#980) — background batcher that coalesces
/// per-compile redb event writes into one fsync per 64 rows / 100 ms.
pub mod event_batcher;
pub mod history_gc;
pub mod ipc;
pub(crate) mod ipc_peer;
pub mod lifecycle;
pub mod maintenance;
pub mod protocol;
pub mod server;
pub mod service_definition;
/// soldr#1838 Phase 1 -- progressive heartbeats so a long daemon wait
/// says what it is waiting on instead of going silent to the backstop.
pub(crate) mod wait_heartbeat;
pub mod wire;
