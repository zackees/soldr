//! `rpprobed` — the probe daemon skeleton (#631, S2 of #628).
//!
//! Scope of this slice: elect a single daemon per user, bind its control
//! socket and loopback HTTP listener, and publish an owner-only discovery
//! file. Registration, capture, and profiling arrive in later slices.
//!
//! # What is reused rather than rebuilt
//!
//! The daemon is **not** a broker backend — it has its own wire
//! (`probe_diag.v1`) and its own lifecycle. But the low-level plumbing is
//! identical to the broker's, so it is borrowed as a library:
//!
//! - framing codec (`broker::protocol::framing`)
//! - peer-credential / owner-only ACL (`broker::server`)
//! - privilege refusal (`broker::lifecycle::privilege`)
//! - private-directory hardening (`broker::secure_dir`)
//!
//! Re-deriving any of these would mean re-deriving their bug fixes too.

#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod bringup;
pub mod capture_jobs;
pub mod cli;
pub mod crash_query;
pub mod crash_store;
pub mod discovery;
pub mod force;
pub mod http;
pub mod names;
pub mod probe_ops;
pub mod profile;
pub mod query;
pub mod registry;
pub mod serve;
pub mod state;
pub mod symbolication;
pub mod wire_convert;

/// Overrides the seeded beacon port, equivalent to `--beacon-port`.
///
/// Declared here rather than written inline at the call site: the workspace's
/// `running-process-env-literal` dylint requires every `RUNNING_PROCESS_*`
/// control to come from a canonical constant, so the set of environment
/// controls stays greppable instead of scattered through string literals.
pub const BEACON_PORT_ENV: &str = "RUNNING_PROCESS_PROBE_BEACON_PORT";

/// Exit code when another instance already owns the endpoint.
///
/// Distinct from a generic failure so a supervisor can tell "already running"
/// (usually fine) from "failed to start" (not fine).
pub const EXIT_ALREADY_BOUND: i32 = 75;

/// Exit code when the daemon refuses to run privileged.
pub const EXIT_PRIVILEGED: i32 = 77;
