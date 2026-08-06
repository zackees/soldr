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

pub use soldr_cache::cache_lib;
pub use soldr_core::{core, self_relocate, timed_test};

/// soldr#2023 — the daemon's single resolution of the compile limit.
pub(crate) mod compile_limit;
pub mod daemon;
pub mod zccache_embedded;
pub(crate) mod zccache_staging;
