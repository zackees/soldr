//! soldr-core — the foundation crate of the #1490 workspace split.
//!
//! Owns the shared types every other soldr crate reaches for: target
//! triple resolution, `~/.soldr/` layout (`SoldrPaths` / `SoldrConfig`),
//! `SoldrError`, the daemon wire schema (`core::wire`), the shared
//! Windows Defender exclusion plumbing, and the `timed_test!` per-test
//! watchdog. No I/O beyond config files.
//!
//! The `core` tree stays nested as `pub mod core;` (not flattened) so
//! `crate::core::…` paths inside the moved files — and
//! `soldr_cli::core::…` paths via the facade re-export — resolve
//! unchanged (#1490 mechanics rule M1).

#![allow(dead_code)]

pub mod broker_identity;
pub mod build_log_meta;
pub mod build_provenance;
pub mod cargo_path_check;
pub mod core;
pub mod defender;
pub mod defender_probe;
pub mod fuzzy_match;
pub mod self_relocate;
pub mod startup_profile;
/// Per-test watchdog (`timed_test!` macro + `run_with_watchdog`).
/// Not cfg-gated: cargo compiles the library *without* `cfg(test)`
/// when linking it into integration tests, so a `cfg(test)` gate
/// would hide the module from `tests/`. The module is tiny and never
/// invoked outside `#[test]` paths.
pub mod test_util;
pub mod warning_log;
