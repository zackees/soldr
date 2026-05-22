//! Internal library re-exports for integration tests under `tests/`.
//!
//! Production code paths run through `src/main.rs`. This `lib.rs`
//! exists only so that the integration tests that survived the
//! four-crate → one-crate collapse can still reach the public
//! functions they exercise (`fetch_tool`, `save`/`load`, etc.) via
//! `use soldr_cli::core::*` / `use soldr_cli::fetch::*` /
//! `use soldr_cli::cache_lib::*`.
//!
//! The bin's module tree (declared in `main.rs`) is compiled
//! independently of this lib tree. Both trees pull from the same
//! source files, so behaviour is identical — there is just an extra
//! rustc invocation for the three folded-in modules.

#![allow(dead_code)]

pub mod cache_lib;
pub mod core;
pub mod fetch;
