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
pub mod cargo_metadata_soldr;
pub mod core;
pub mod daemon;
pub mod defender;
pub mod fetch;
pub mod release_sidecar;
pub mod self_relocate;
/// `wrapper_target` holds the wrapper hot-path target-registry routing
/// extracted out of `wrapper.rs` so integration tests under
/// `tests/cli_wrapper_perf.rs` can drive it in-process (issue #474).
/// The bin tree declares the same module via `main.rs`.
pub mod wrapper_target;
/// Issue #977 — embedded zccache service wrapper. Gated on the
/// `embedded` Cargo feature; absent the feature this module is not
/// compiled and the daemon's `CompileBackend` enum only has the
/// `Wrapped` variant.
#[cfg(feature = "embedded")]
pub mod zccache_embedded;
pub mod zccache_lifecycle;

/// Per-test watchdog (`timed_test!` macro + `run_with_watchdog`).
/// Exposed from the lib tree so both unit tests in `src/` and
/// integration tests under `tests/` can reach it as
/// `soldr_cli::test_util::*`. Not cfg-gated because cargo compiles the
/// library *without* `cfg(test)` when linking it into integration
/// tests, so a `cfg(test)` gate would silently hide the module from
/// `tests/`. The module is tiny (one function + one constant) and is
/// never invoked outside `#[test]` paths, so the production-binary
/// cost is negligible.
pub mod test_util;
