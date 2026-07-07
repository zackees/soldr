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

/// soldr#1012 PR 5 — blessed cross-compile sysroot prep called from
/// `Commands::Build` for canonical target triples. Coordinates the
/// xwin-cache materialization + clang-shim install + env var setup.
pub mod blessed_build;
pub mod cache_lib;
pub mod cargo_metadata_soldr;
/// soldr#1059 — classify the `cargo` binary that `which cargo`
/// resolves to. Lib-side declaration mirrors `main.rs` so integration
/// tests can exercise the classifier directly.
pub mod cargo_path_check;
/// soldr#1081 — Shared `Request::Compile` dispatch logic used by both
/// the soldr-as-RUSTC_WRAPPER hot path (`wrapper.rs`) and multicall
/// `zccache-soldr` dispatch. Owns the hang-safe retry budget contract.
pub mod compile_dispatch;
pub mod core;
pub mod daemon;
pub mod defender;
/// soldr#938 — `soldr env --target` subcommand implementation.
/// Declared from both lib and bin so the unit tests are reachable
/// via `cargo test --lib`.
pub mod env_cmd;
/// soldr#1059 — `soldr exec <cmd>` PATH-prepend wrapper. Lib-side
/// declaration mirrors `main.rs` so the unit tests are reachable via
/// `cargo test --lib`.
pub mod exec_cmd;
pub mod fetch;
/// soldr#820 — `soldr logs` discoverable runtime-log surface.
/// Lib-side declaration mirrors `main.rs` so unit tests are reachable
/// via `cargo test --lib`.
pub mod logs_cmd;
/// soldr#1079 — Windows MSVC host-toolchain auto-discovery. Probes
/// vswhere + the Windows SDK and synthesizes LIB/INCLUDE/PATH/LIBPATH
/// onto the current process so `soldr cargo build` / `soldr cargo test`
/// succeed from a plain PowerShell without the downstream `$env:LIB`
/// workaround. Detect-host now; managed-catalogue fallback is a
/// follow-up in the same issue.
pub mod msvc_host;
pub mod multicall;
/// soldr#939 — PyO3 auto-detection via cargo metadata.
pub mod pyo3_detect;
pub mod release_sidecar;
pub mod self_relocate;
pub mod shim_materialize;
/// soldr#997 — friendly target aliases + Rust-triple passthrough.
/// See module doc for the `soldr build --target <alias>` UX contract.
pub mod target_alias;
/// `wrapper_target` holds the wrapper hot-path target-registry routing
/// extracted out of `wrapper.rs` so integration tests under
/// `tests/cli_wrapper_perf.rs` can drive it in-process (issue #474).
/// The bin tree declares the same module via `main.rs`.
pub mod wrapper_target;
/// Issue #977 / #980 L1 — embedded zccache service wrapper. The
/// daemon always links the embedded service; the legacy
/// fork-zccache.exe wrapper path has been deleted. The standalone
/// managed zccache binary is still used by `soldr update-zccache`
/// and the perf-cluster broker, but those paths are independent of
/// this module.
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
