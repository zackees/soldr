//! soldr-cache — the `RUSTC_WRAPPER` cache library (#1490 Phase 4).
//!
//! Owns `cache_lib`: input hashing, `~/.soldr/cache/` layout, the
//! `soldr save` / `soldr load` archive transport, and the auto-GC
//! orchestrator. The tree stays nested as `pub mod cache_lib;` (not
//! flattened) so `crate::cache_lib::…` paths inside the moved files —
//! and `soldr_cli::cache_lib::…` via the facade re-export — resolve
//! unchanged (#1490 mechanics rule M1). The re-exports below satisfy
//! the tree's `crate::core::…` / `crate::defender::…` paths through
//! the crate root (rule M2).

#![allow(dead_code)]

/// Neutral host-platform facade (#2493): the single selection site lives
/// in `soldr-platform`; this crate calls only `crate::platform::…`.
pub(crate) use soldr_platform as platform;

pub use soldr_core::{core, defender, timed_test};

pub mod cache_lib;
