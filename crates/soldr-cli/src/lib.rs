//! The soldr library crate — the single compilation of the whole
//! module tree (#1490 Phase 1).
//!
//! Before Phase 1 the bin tree (`main.rs`) and this lib tree each
//! declared the same ~40K LOC of modules, so every build compiled
//! them twice. Now every module is declared exactly once, here;
//! `src/main.rs` is a thin shim that calls [`run`], and the CLI entry
//! logic itself lives in [`soldr_main`], glob-re-exported at the crate
//! root so historical `crate::<item>` paths keep resolving.
//!
//! Integration tests under `tests/` keep their
//! `use soldr_cli::core::*` / `soldr_cli::fetch::*` /
//! `soldr_cli::cache_lib::*` imports unchanged.

#![allow(dead_code, unused_imports)]

pub mod archive_cmd;
pub mod binaries;
/// soldr#1012 PR 5 — blessed cross-compile sysroot prep called from
/// `Commands::Build` for canonical target triples. Coordinates the
/// xwin-cache materialization + clang-shim install + env var setup.
pub mod blessed_build;
pub mod bootstrap;
pub mod build_from_source_cmd;
pub mod cache;
pub mod cache_lib;
pub mod cargo_diagnostics;
pub mod cargo_front_door;
pub mod cargo_metadata_soldr;
/// soldr#1059 — classify the `cargo` binary that `which cargo`
/// resolves to. Integration tests exercise the classifier directly.
pub mod cargo_path_check;
pub mod cli_args;
pub mod cli_dispatch;
/// soldr#1081 — Shared `Request::Compile` dispatch logic used by both
/// the soldr-as-RUSTC_WRAPPER hot path (`wrapper.rs`) and multicall
/// `zccache-soldr` dispatch. Owns the hang-safe retry budget contract.
pub mod compile_dispatch;
pub mod cook;
pub mod core;
pub mod daemon;
pub mod defender;
pub mod defender_probe;
pub mod doctor;
/// soldr#938 — `soldr env --target` subcommand implementation.
pub mod env_cmd;
/// soldr#1059 — `soldr exec <cmd>` PATH-prepend wrapper.
pub mod exec_cmd;
pub mod fetch;
pub mod fuzzy_match;
pub mod gc;
pub mod install_shims;
pub mod linker;
/// soldr#820 — `soldr logs` discoverable runtime-log surface.
pub mod logs_cmd;
/// soldr#1079 — Windows MSVC host-toolchain auto-discovery. Probes
/// vswhere + the Windows SDK and synthesizes LIB/INCLUDE/PATH/LIBPATH
/// onto the current process so `soldr cargo build` / `soldr cargo test`
/// succeed from a plain PowerShell without the downstream `$env:LIB`
/// workaround. Detect-host now; managed-catalogue fallback is a
/// follow-up in the same issue.
pub mod msvc_host;
pub mod multicall;
pub mod native_cc;
pub mod optimize;
pub mod optimize_detect;
pub mod optimize_windows;
pub mod prepare_cmd;
/// soldr#939 — PyO3 auto-detection via cargo metadata.
pub mod pyo3_detect;
pub mod release_sidecar;
pub mod rust_plan;
pub mod save_load;
pub mod self_relocate;
pub mod shim_dir;
pub mod shim_materialize;
pub mod startup_profile;
/// soldr#997 — friendly target aliases + Rust-triple passthrough.
/// See module doc for the `soldr build --target <alias>` UX contract.
pub mod target_alias;
pub mod toolchain;
pub mod toolchain_doctor;
pub mod toolchain_ensure;
pub mod toolchain_link;
pub mod trampoline;
pub mod trampoline_workspace;
pub mod wrapper;
/// `wrapper_target` holds the wrapper hot-path target-registry routing
/// extracted out of `wrapper.rs` so integration tests under
/// `tests/cli_wrapper_perf.rs` can drive it in-process (issue #474).
pub mod wrapper_target;
pub mod zccache;
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

/// CLI entry logic — formerly the `main.rs` crate root (#1490 Phase 1).
mod soldr_main;

/// The full soldr CLI entry point, called by the `src/main.rs` shim.
pub use soldr_main::run;
/// Historical `crate::<item>` paths (consts, dispatch helpers) resolve
/// through this glob just as they did when `main.rs` was the crate
/// root.
pub(crate) use soldr_main::*;
