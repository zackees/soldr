//! soldr-daemon — the daemon runtime crate (#1490 Phase 5).
//!
//! Owns `daemon` (lifecycle, IPC server, wire codec, running-process v2
//! broker adoption, displacement) and `zccache_embedded` (the in-process
//! zccache service the daemon hosts — mutual edge with `daemon`, so the
//! pair moves together). Both trees stay nested as `pub mod` (not
//! flattened) so `crate::daemon::…` / `crate::zccache_embedded::…`
//! paths inside the moved files — and `soldr_cli::daemon::…` via the
//! facade re-export — resolve unchanged (#1490 mechanics rule M1). The
//! re-exports below satisfy the trees' `crate::core::…` /
//! `crate::self_relocate::…` / `crate::cache_lib::…` paths through the
//! crate root (rule M2).

#![allow(dead_code)]

/// Neutral host-platform facade (#2493): the single selection site lives
/// in `soldr-platform`; this crate calls only `crate::platform::…`.
pub(crate) use soldr_platform as platform;

pub use soldr_cache::cache_lib;
pub use soldr_core::{core, self_relocate};

/// soldr#2023 — the daemon's single resolution of the compile limit.
pub(crate) mod amalgamation;
pub(crate) mod ci_test_report;
pub(crate) mod compile_limit;
mod compiler_exit;
pub mod daemon;
pub(crate) mod oom_evidence;
pub mod zccache_embedded;
pub(crate) mod zccache_staging;
