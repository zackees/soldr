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
pub mod cargo_diagnostics;
pub mod cargo_front_door;
pub mod cargo_metadata_soldr;
pub mod cli_args;
pub mod cli_dispatch;
/// soldr#1081 — Shared `Request::Compile` dispatch logic used by both
/// the soldr-as-RUSTC_WRAPPER hot path (`wrapper.rs`) and multicall
/// `zccache-soldr` dispatch. Owns the hang-safe retry budget contract.
pub mod compile_dispatch;
pub mod cook;
pub mod doctor;
/// soldr#938 — `soldr env --target` subcommand implementation.
pub mod env_cmd;
/// soldr#1059 — `soldr exec <cmd>` PATH-prepend wrapper.
pub mod exec_cmd;
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
pub mod shim_dir;
pub mod shim_materialize;
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
pub mod zccache_lifecycle;

// #1490 Phase 2 facade (mechanics rule M3): every module that was
// ever a `mod` in soldr-cli stays reachable as `soldr_cli::<name>` /
// `crate::<name>` via these re-exports of the extracted soldr-core
// crate. `timed_test` is the `#[macro_export]` watchdog macro.
pub use soldr_cache::cache_lib;
pub use soldr_core::{
    cargo_path_check, core, defender, defender_probe, fuzzy_match, self_relocate, startup_profile,
    test_util, timed_test,
};
pub use soldr_daemon::{daemon, zccache_embedded};
pub use soldr_fetch::fetch;

/// CLI entry logic — formerly the `main.rs` crate root (#1490 Phase 1).
mod soldr_main;

/// The full soldr CLI entry point, called by the `src/main.rs` shim.
pub use soldr_main::run;
/// Historical `crate::<item>` paths (consts, dispatch helpers) resolve
/// through this glob just as they did when `main.rs` was the crate
/// root.
pub(crate) use soldr_main::*;
